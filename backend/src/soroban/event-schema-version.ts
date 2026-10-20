import {
  CONTRACT_EVENT_SCHEMA_VERSION,
  LEGACY_CONTRACT_EVENT_SCHEMA_VERSION,
  SUPPORTED_CONTRACT_EVENT_SCHEMA_VERSIONS,
  GENERATED_CONTRACT_EVENT_SCHEMA_REGISTRY,
} from './generated/contract-types';

export {
  CONTRACT_EVENT_SCHEMA_VERSION,
  LEGACY_CONTRACT_EVENT_SCHEMA_VERSION,
  SUPPORTED_CONTRACT_EVENT_SCHEMA_VERSIONS,
};

type EventLike = {
  eventData?: Record<string, unknown> | null;
  topics?: unknown[] | null;
};

export class UnsupportedContractEventSchemaVersionError extends Error {
  constructor(version: number) {
    super(`Unsupported contract event schema version: ${version}`);
    this.name = UnsupportedContractEventSchemaVersionError.name;
  }
}

export function getContractEventSchemaVersion(event: EventLike): number {
  const payloadVersion =
    event.eventData?.schemaVersion ?? event.eventData?.schema_version;

  if (payloadVersion !== undefined) {
    return normalizeSchemaVersion(payloadVersion);
  }

  const topicVersion = event.topics?.[event.topics.length - 1];
  if (topicVersion !== undefined) {
    const parsed = parseTopicVersion(topicVersion);
    if (parsed !== undefined) {
      return parsed;
    }
  }

  return LEGACY_CONTRACT_EVENT_SCHEMA_VERSION;
}

export function assertSupportedContractEventSchemaVersion(
  event: EventLike,
): number {
  const version = getContractEventSchemaVersion(event);
  if (
    !(SUPPORTED_CONTRACT_EVENT_SCHEMA_VERSIONS as readonly number[]).includes(
      version,
    )
  ) {
    throw new UnsupportedContractEventSchemaVersionError(version);
  }
  return version;
}

// ── Schema registry ───────────────────────────────────────────────────
//
// The registry validates/decodes each contract event topic against the
// schema versions the backend knows how to process. Events whose topic is
// unknown, whose schema version is unsupported, or whose payload fails
// structural validation are reported as *undecodable* rather than throwing,
// so the caller can quarantine them into `raw_unparsed_events` instead of
// dropping them or crashing ingestion.

export type ContractEventEnvelope = EventLike & {
  eventType?: string | null;
};

export enum ContractEventDecodeFailureReason {
  MALFORMED_ENVELOPE = 'malformed_envelope',
  UNREGISTERED_EVENT_TYPE = 'unregistered_event_type',
  UNSUPPORTED_SCHEMA_VERSION = 'unsupported_schema_version',
  SCHEMA_VALIDATION_FAILED = 'schema_validation_failed',
}

export interface ContractEventSchemaEntry {
  /** Schema versions the backend can decode for this event type. */
  versions: readonly number[];
  /** Payload keys that must be present for the event to be considered decodable. */
  requiredFields?: readonly string[];
}

/**
 * Known contract event topics and the schema versions the indexer supports.
 *
 * Sourced from `./generated/contract-types`, which is code-generated from the
 * committed contract spec XDR snapshots (see `scripts/generate-contract-types`).
 * Do not hand-edit event shapes here — update the contract spec snapshot and
 * regenerate so the backend, contracts and coordinator share one source of
 * truth. Adding a new event type or version to the generated registry is the
 * single place that opts the indexer into processing it — anything else is
 * quarantined.
 */
export const CONTRACT_EVENT_SCHEMA_REGISTRY: Readonly<
  Record<string, ContractEventSchemaEntry>
> = GENERATED_CONTRACT_EVENT_SCHEMA_REGISTRY;

export type ContractEventDecodeResult =
  | {
      ok: true;
      eventType: string;
      version: number;
      data: Record<string, unknown>;
    }
  | {
      ok: false;
      eventType: string | null;
      version: number | null;
      reason: ContractEventDecodeFailureReason;
      detail: string;
    };

/** Resolve the event type from an explicit field or the Soroban topic tuple. */
export function resolveContractEventType(
  event: ContractEventEnvelope,
): string | null {
  if (typeof event.eventType === 'string' && event.eventType.length > 0) {
    return event.eventType;
  }

  const topics = event.topics ?? [];
  // EVENTS.md envelope: ["<domain>", "<event_type>", <schema_version>]
  const candidate =
    topics.length >= 2 ? topics[topics.length - 2] : topics[topics.length - 1];
  return typeof candidate === 'string' && candidate.length > 0
    ? candidate
    : null;
}

/**
 * Decode a raw contract event against the schema registry.
 * Never throws — undecodable events return `{ ok: false, reason }`.
 */
export function decodeContractEvent(
  event: ContractEventEnvelope,
): ContractEventDecodeResult {
  const eventType = resolveContractEventType(event);
  if (!eventType) {
    return {
      ok: false,
      eventType: null,
      version: null,
      reason: ContractEventDecodeFailureReason.MALFORMED_ENVELOPE,
      detail: 'Event is missing an eventType and a usable topic tuple',
    };
  }

  let version: number;
  try {
    version = getContractEventSchemaVersion(event);
  } catch {
    return {
      ok: false,
      eventType,
      version: null,
      reason: ContractEventDecodeFailureReason.UNSUPPORTED_SCHEMA_VERSION,
      detail: 'Schema version marker could not be parsed',
    };
  }

  const entry = CONTRACT_EVENT_SCHEMA_REGISTRY[eventType];
  if (!entry) {
    return {
      ok: false,
      eventType,
      version,
      reason: ContractEventDecodeFailureReason.UNREGISTERED_EVENT_TYPE,
      detail: `No schema registered for event type '${eventType}'`,
    };
  }

  if (!entry.versions.includes(version)) {
    return {
      ok: false,
      eventType,
      version,
      reason: ContractEventDecodeFailureReason.UNSUPPORTED_SCHEMA_VERSION,
      detail: `Schema v${version} is not supported for event type '${eventType}'`,
    };
  }

  const data = event.eventData ?? {};
  const missing = (entry.requiredFields ?? []).filter(
    (field) => data[field] === undefined || data[field] === null,
  );
  if (missing.length > 0) {
    return {
      ok: false,
      eventType,
      version,
      reason: ContractEventDecodeFailureReason.SCHEMA_VALIDATION_FAILED,
      detail: `Missing required payload field(s): ${missing.join(', ')}`,
    };
  }

  return { ok: true, eventType, version, data };
}

function parseTopicVersion(topic: unknown): number | undefined {
  // EVENTS.md envelope (lifebank-soroban contracts, schema catalog v1+):
  // the trailing topic segment is a raw u32, decoded here as a plain number.
  if (typeof topic === 'number' && Number.isInteger(topic)) {
    return topic;
  }

  // Legacy convention: trailing topic segment was a `vN` symbol.
  if (typeof topic !== 'string') {
    return undefined;
  }

  const match = /^v(\d+)$/.exec(topic);
  return match ? Number(match[1]) : undefined;
}

function normalizeSchemaVersion(value: unknown): number {
  if (typeof value === 'number' && Number.isInteger(value)) {
    return value;
  }

  if (typeof value === 'string') {
    const trimmed = value.trim();
    const prefixed = parseTopicVersion(trimmed);
    if (prefixed !== undefined) {
      return prefixed;
    }

    const parsed = Number(trimmed);
    if (Number.isInteger(parsed)) {
      return parsed;
    }
  }

  throw new UnsupportedContractEventSchemaVersionError(Number.NaN);
}
