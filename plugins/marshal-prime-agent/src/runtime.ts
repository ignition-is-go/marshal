/** The small part of Prime Agent's session header used for registration. */
export interface PrimeAgentSessionHeader {
  rlmDepth?: unknown;
  [key: string]: unknown;
}

function isPositiveIntegerDepth(value: unknown): boolean {
  return typeof value === "number" && Number.isInteger(value) && value > 0;
}

/** Return whether a canonical Prime Agent session depth belongs to an RLM child. */
export function isPrimeAgentSubagent(rlmDepth: unknown): boolean {
  return isPositiveIntegerDepth(rlmDepth);
}

/** Public-API guard for the session_start registration boundary. */
export function shouldRegisterPrimeAgentSession(header: PrimeAgentSessionHeader | null): boolean {
  return !isPrimeAgentSubagent(header?.rlmDepth);
}
