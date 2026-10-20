#![no_std]

mod error;
mod events;
mod storage;
mod types;

#[cfg(test)]
mod test;

pub use crate::error::ContractError;
pub use crate::types::{
    ContractMetadata, DataKey, RequestCreatedEvent,
};
pub use interfaces::{
    BloodComponent, BloodRequest, BloodType, InventoryContractClient, RequestStatus, Urgency,
};

mod validation;

use soroban_sdk::{contract, contractimpl, Address, Env};

#[contract]
pub struct RequestContract;

#[contractimpl]
impl RequestContract {
    /// Atomic constructor — deploy + init in a single transaction.
    pub fn __constructor(
        env: Env,
        admin: Address,
        inventory_contract: Address,
    ) {
        admin.require_auth();
        if storage::is_initialized(&env) {
            panic!("already initialized");
        }
        storage::set_admin(&env, &admin);
        storage::set_inventory_contract(&env, &inventory_contract);
        storage::set_request_counter(&env, 0);
        storage::set_metadata(&env, &storage::default_metadata(&env));
        storage::authorize_hospital(&env, &admin);
        storage::set_initialized(&env);
        env.storage()
            .instance()
            .set(&SCHEMA_VERSION_KEY, &TARGET_SCHEMA_VERSION);
        events::emit_initialized(&env, &admin, &inventory_contract);
    }

    pub fn initialize(
        env: Env,
        admin: Address,
        inventory_contract: Address,
    ) -> Result<(), ContractError> {
        admin.require_auth();

        if storage::is_initialized(&env) {
            return Err(ContractError::AlreadyInitialized);
        }

        storage::set_admin(&env, &admin);
        storage::set_inventory_contract(&env, &inventory_contract);
        storage::set_request_counter(&env, 0);
        storage::set_metadata(&env, &storage::default_metadata(&env));
        storage::authorize_hospital(&env, &admin);
        storage::set_initialized(&env);

        events::emit_initialized(&env, &admin, &inventory_contract);

        Ok(())
    }

    pub fn authorize_hospital(env: Env, hospital: Address) -> Result<(), ContractError> {
        storage::require_initialized(&env)?;
        storage::get_admin(&env).require_auth();
        storage::authorize_hospital(&env, &hospital);
        Ok(())
    }

    pub fn revoke_hospital(env: Env, hospital: Address) -> Result<(), ContractError> {
        storage::require_initialized(&env)?;
        storage::get_admin(&env).require_auth();
        storage::revoke_hospital(&env, &hospital);
        Ok(())
    }

    pub fn create_request(
        env: Env,
        hospital: Address,
        blood_type: BloodType,
        component: BloodComponent,
        quantity_ml: u32,
        urgency: Urgency,
        required_by_timestamp: u64,
    ) -> Result<u64, ContractError> {
        hospital.require_auth();
        storage::require_initialized(&env)?;

        if !storage::is_hospital_authorized(&env, &hospital) {
            return Err(ContractError::NotAuthorizedHospital);
        }

        validation::validate_timestamp(&env, required_by_timestamp)?;
        validation::validate_quantity(quantity_ml)?;

        let request_id = storage::increment_request_counter(&env);
        let request = BloodRequest {
            id: request_id,
            hospital_id: hospital.clone(),
            blood_type,
            component,
            quantity_ml,
            urgency,
            created_timestamp: env.ledger().timestamp(),
            required_by_timestamp,
            status: RequestStatus::Pending,
            assigned_units: soroban_sdk::Vec::new(&env),
            fulfilled_quantity_ml: 0,
            reservation_id: None,
        };

        storage::set_request(&env, &request);
        events::emit_request_created(&env, &request);

        Ok(request_id)
    }

    /// Create multiple blood requests in a single transaction.
    /// Each tuple is `(blood_type, component, quantity_ml, urgency, required_by_timestamp)`.
    /// Returns the Vec of new request IDs in input order.
    pub fn batch_create_requests(
        env: Env,
        hospital: Address,
        entries: soroban_sdk::Vec<(BloodType, BloodComponent, u32, Urgency, u64)>,
    ) -> Result<soroban_sdk::Vec<u64>, ContractError> {
        hospital.require_auth();
        storage::require_initialized(&env)?;

        if !storage::is_hospital_authorized(&env, &hospital) {
            return Err(ContractError::NotAuthorizedHospital);
        }

        let mut ids: soroban_sdk::Vec<u64> = soroban_sdk::Vec::new(&env);
        for i in 0..entries.len() {
            let (blood_type, component, quantity_ml, urgency, required_by_timestamp) =
                entries.get(i).unwrap();
            validation::validate_timestamp(&env, required_by_timestamp)?;
            validation::validate_quantity(quantity_ml)?;

            let request_id = storage::increment_request_counter(&env);
            let request = BloodRequest {
                id: request_id,
                hospital_id: hospital.clone(),
                blood_type,
                component,
                quantity_ml,
                urgency,
                created_timestamp: env.ledger().timestamp(),
                required_by_timestamp,
                status: RequestStatus::Pending,
                assigned_units: soroban_sdk::Vec::new(&env),
                fulfilled_quantity_ml: 0,
                reservation_id: None,
            };
            storage::set_request(&env, &request);
            events::emit_request_created(&env, &request);
            ids.push_back(request_id);
        }
        Ok(ids)
    }

    /// Cancel a blood request. Only the owning hospital or the admin may cancel.
    /// The request must be in Pending or Approved status.
    pub fn cancel_request(
        env: Env,
        caller: Address,
        request_id: u64,
    ) -> Result<(), ContractError> {
        caller.require_auth();
        storage::require_initialized(&env)?;

        let mut request = storage::get_request(&env, request_id)
            .ok_or(ContractError::RequestNotFound)?;

        let admin = storage::get_admin(&env);
        if caller != request.hospital_id && caller != admin {
            return Err(ContractError::NotRequestOwner);
        }

        match request.status {
            RequestStatus::Pending | RequestStatus::Approved => {}
            _ => return Err(ContractError::InvalidRequestStatus),
        }

        request.status = RequestStatus::Cancelled;
        storage::set_request(&env, &request);

        if let Some(res_id) = request.reservation_id {
            let inventory_addr = storage::get_inventory_contract(&env);
            let inv_client = InventoryContractClient::new(&env, &inventory_addr);
            inv_client.release_reservation(&res_id);
        }

        events::emit_request_cancelled(
            &env,
            request_id,
            &caller,
            env.ledger().timestamp(),
        );

        Ok(())
    }

    /// Update the status of a blood request. Admin only.
    /// Records the caller as the actor in the emitted event.
    pub fn update_request_status(
        env: Env,
        caller: Address,
        request_id: u64,
        new_status: RequestStatus,
    ) -> Result<(), ContractError> {
        caller.require_auth();
        storage::require_initialized(&env)?;

        let admin = storage::get_admin(&env);
        if caller != admin {
            return Err(ContractError::Unauthorized);
        }

        let mut request = storage::get_request(&env, request_id)
            .ok_or(ContractError::RequestNotFound)?;

        if request.status == new_status {
            return Err(ContractError::InvalidRequestStatus);
        }

        let old_status = request.status;
        request.status = new_status;
        storage::set_request(&env, &request);

        events::emit_request_status_updated(
            &env,
            request_id,
            &caller,
            old_status,
            new_status,
            env.ledger().timestamp(),
        );

        Ok(())
    }

    pub fn get_request(env: Env, request_id: u64) -> Result<BloodRequest, ContractError> {
        storage::require_initialized(&env)?;
        storage::get_request(&env, request_id).ok_or(ContractError::RequestNotFound)
    }

    pub fn get_admin(env: Env) -> Result<Address, ContractError> {
        storage::require_initialized(&env)?;
        Ok(storage::get_admin(&env))
    }

    pub fn get_inventory_contract(env: Env) -> Result<Address, ContractError> {
        storage::require_initialized(&env)?;
        Ok(storage::get_inventory_contract(&env))
    }

    pub fn get_request_counter(env: Env) -> Result<u64, ContractError> {
        storage::require_initialized(&env)?;
        Ok(storage::get_request_counter(&env))
    }

    pub fn get_metadata(env: Env) -> Result<ContractMetadata, ContractError> {
        storage::require_initialized(&env)?;
        Ok(storage::get_metadata(&env))
    }

    pub fn is_hospital_authorized(env: Env, hospital: Address) -> bool {
        storage::is_hospital_authorized(&env, &hospital)
    }

    pub fn is_initialized(env: Env) -> bool {
        storage::is_initialized(&env)
    }

    // ── Upgradeability & versioned storage schema (#31) ──────────────────────

    /// Code version of the currently deployed binary.
    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    /// Storage schema version currently recorded on-chain (1 when unset).
    pub fn schema_version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&SCHEMA_VERSION_KEY)
            .unwrap_or(1)
    }

    /// Replace the running WASM with an already-installed hash. Admin only.
    /// The contract ID and all storage are preserved; call `migrate` after
    /// the upgrade when the new binary bumps `TARGET_SCHEMA_VERSION`.
    pub fn upgrade(env: Env, new_wasm_hash: soroban_sdk::BytesN<32>) -> Result<(), ContractError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&crate::types::DataKey::Admin)
            .ok_or(ContractError::NotInitialized)?;
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
        Ok(())
    }

    /// Apply version-gated storage migrations after an upgrade. Admin only.
    /// Refuses to run once storage already sits at `TARGET_SCHEMA_VERSION`,
    /// so a migration can never be applied twice.
    pub fn migrate(env: Env) -> Result<u32, ContractError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&crate::types::DataKey::Admin)
            .ok_or(ContractError::NotInitialized)?;
        admin.require_auth();
        let current = Self::schema_version(env.clone());
        if current >= TARGET_SCHEMA_VERSION {
            return Err(ContractError::MigrationAlreadyApplied);
        }
        // Version-gated transformations run here as the schema evolves, e.g.
        // `if current < 2 { /* rewrite v1 entries into the v2 layout */ }`.
        env.storage()
            .instance()
            .set(&SCHEMA_VERSION_KEY, &TARGET_SCHEMA_VERSION);
        Ok(TARGET_SCHEMA_VERSION)
    }
}

// ── Upgradeability & versioned storage schema (#31) ───────────────────────────
//
// Invariant: after an upgrade, the new binary must be able to read every
// prior storage schema version until `migrate` has completed. Absence of the
// stored schema version means schema 1.

/// Code version compiled into this binary. Bump on every release.
pub const CONTRACT_VERSION: u32 = 1;

/// Storage schema version this binary writes. Bump only together with a
/// version-gated transformation in `migrate`.
pub const TARGET_SCHEMA_VERSION: u32 = 1;

const SCHEMA_VERSION_KEY: soroban_sdk::Symbol = soroban_sdk::symbol_short!("SCHEMA_V");
