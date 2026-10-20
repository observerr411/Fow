use soroban_sdk::{contracttype, Address, String, Vec};

// Re-export cross-contract types from the shared interfaces crate so that
// there is a single, compiler-enforced definition shared with every caller.
pub use interfaces::{
    BloodRequest, BloodType, RequestCreatedEvent, RequestStatus,
};

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub enum DataKey {
    Admin,
    InventoryContract,
    RequestCounter,
    Initialized,
    Metadata,
    AuthorizedHospital(Address),
    Request(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct ContractMetadata {
    pub name: String,
    pub version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[contracttype]
pub enum BloodComponent {
    WholeBlood,
    RedCells,
    Plasma,
    Platelets,
    Cryoprecipitate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[contracttype]
pub enum Urgency {
    Critical,
    Urgent,
    Routine,
    Scheduled,
}

impl Urgency {
    pub fn priority(&self) -> u32 {
        match self {
            Self::Critical => 4,
            Self::Urgent => 3,
            Self::Routine => 2,
            Self::Scheduled => 1,
        }
    }
}