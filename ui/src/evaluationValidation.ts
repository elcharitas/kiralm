export function positiveInteger(
  value: string,
  label: string,
  maximum = Number.MAX_SAFE_INTEGER,
): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0 || parsed > maximum) {
    throw new Error(`${label} must be a positive whole number`);
  }
  return parsed;
}

export function unusedNumberedId(
  ids: readonly string[],
  prefix: string,
): string {
  const used = new Set(ids.map((id) => id.trim()));
  let index = 1;
  while (used.has(`${prefix}-${index}`)) index += 1;
  return `${prefix}-${index}`;
}

export function validateAssertionValue(type: string, value: string) {
  if ((type === "contains" || type === "regex") && !value.trim()) {
    throw new Error(
      "Contains and regular expression assertions cannot be blank",
    );
  }
}
