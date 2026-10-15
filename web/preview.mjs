// Generic presentation of a .program-generated file. Decisions stay in the script.
export function decodePreview(raw) {
  if (typeof raw !== 'string' || raw.length > 16384 || new TextEncoder().encode(raw).length > 16384)
    throw new Error('Dashboard artifact exceeds 16 KiB');
  const value = JSON.parse(raw);
  const text = (item, limit) => typeof item === 'string' && item.length <= limit;
  if (
    !value ||
    Array.isArray(value) ||
    typeof value !== 'object' ||
    !text(value.title, 120) ||
    !text(value.status, 80)
  )
    throw new Error('Dashboard requires a title and status');
  const metrics = value.metrics ?? [];
  const notes = value.notes ?? [];
  if (
    !Array.isArray(metrics) ||
    metrics.length > 8 ||
    metrics.some(
      (item) =>
        !item ||
        !text(item.label, 60) ||
        !Number.isSafeInteger(item.value) ||
        item.value < 0 ||
        item.value > 1000000000,
    )
  )
    throw new Error('Dashboard metrics require up to eight labeled nonnegative integers');
  if (!Array.isArray(notes) || notes.length > 8 || notes.some((item) => !text(item, 200)))
    throw new Error('Dashboard notes require up to eight short strings');
  // Show direction overrides as visible escapes; keep the saved artifact intact.
  const display = (item) =>
    item.replace(
      /[\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/gu,
      (character) => `\\u${character.codePointAt(0).toString(16).padStart(4, '0')}`,
    );
  return {
    title: display(value.title),
    status: display(value.status),
    metrics: metrics.map(({ label, value }) => ({ label: display(label), value })),
    notes: notes.map(display),
  };
}
