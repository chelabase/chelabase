import type { FunctionSchedule } from "@chelabase/management-client";

/** Each cron field: its name, range, and the names it may use (months and weekdays). */
const FIELDS: { name: string; min: number; max: number; names?: string[] }[] = [
  { name: "minute", min: 0, max: 59 },
  { name: "hour", min: 0, max: 23 },
  { name: "day of month", min: 1, max: 31 },
  { name: "month", min: 1, max: 12, names: ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"] },
  // 7 is Sunday too.
  { name: "day of week", min: 0, max: 7, names: ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"] },
];

/** Whether one comma-separated item of a field is well formed and in range. */
function itemOk(item: string, f: (typeof FIELDS)[number]): boolean {
  const [range, step, ...extra] = item.split("/");
  if (extra.length > 0) return false;
  if (step !== undefined && !(/^\d+$/.test(step) && Number(step) >= 1)) return false;
  if (range === "*" || (range === "?" && f.names !== undefined && f.max === 7)) return true;
  const value = (v: string): number | null => {
    if (/^\d+$/.test(v)) return Number(v);
    const i = f.names?.indexOf(v.toUpperCase()) ?? -1;
    return i < 0 ? null : f.max === 12 ? i + 1 : i;
  };
  const bounds = range.split("-");
  if (bounds.length > 2) return false;
  const values = bounds.map(value);
  if (values.some((v) => v === null || v < f.min || v > f.max)) return false;
  return values.length === 1 || (values[0] as number) <= (values[1] as number);
}

/**
 * Why a cron can't be sent (null: it can). Catches the obvious: not 5 fields, stray characters,
 * a number out of a field's range. The API decides the rest.
 */
export function cronReason(raw: string): string | null {
  const cron = raw.trim();
  if (cron === "") return "Enter a cron, e.g. */15 * * * *.";
  const fields = cron.split(/\s+/);
  if (fields.length !== 5) return "A cron has 5 fields: minute, hour, day of month, month, day of week.";
  for (let i = 0; i < 5; i++) {
    const f = FIELDS[i];
    if (!fields[i].split(",").every((item) => item !== "" && itemOk(item, f))) {
      return `The ${f.name} field "${fields[i]}" isn't valid (${f.min}-${f.max}${f.names ? " or names" : ""}).`;
    }
  }
  return null;
}

/** Why a time zone can't be sent: blank, or one this browser doesn't know as IANA. */
export function timezoneReason(raw: string): string | null {
  const tz = raw.trim();
  if (tz === "") return "Enter a time zone, e.g. UTC or Europe/London.";
  try {
    new Intl.DateTimeFormat("en", { timeZone: tz });
    return null;
  } catch {
    return `${tz} isn't a time zone (use an IANA name such as Europe/London).`;
  }
}

/** The schedule form: a new schedule (`editing` null) or one being edited. */
export interface ScheduleForm {
  editing: string | null;
  cron: string;
  timezone: string;
}

export type ScheduleFormAction =
  | { type: "edit"; schedule: FunctionSchedule }
  | { type: "set"; field: "cron" | "timezone"; value: string }
  | { type: "reset" };

export function initialScheduleForm(): ScheduleForm {
  return { editing: null, cron: "", timezone: "UTC" };
}

export function scheduleFormReducer(state: ScheduleForm, action: ScheduleFormAction): ScheduleForm {
  switch (action.type) {
    case "edit":
      return { editing: action.schedule.id, cron: action.schedule.cron, timezone: action.schedule.timezone };
    case "set":
      return { ...state, [action.field]: action.value };
    case "reset":
      return initialScheduleForm();
  }
}

export function scheduleFormReason(form: ScheduleForm): string | null {
  return cronReason(form.cron) ?? timezoneReason(form.timezone);
}
