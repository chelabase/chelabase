import { describe, expect, it } from "vitest";
import { cronReason, scheduleStatus, initialScheduleForm, scheduleFormReducer, scheduleFormReason, timezoneReason } from "./functions-schedules";

describe("cronReason", () => {
  it("accepts_common_crons", () => {
    for (const cron of ["* * * * *", "*/15 * * * *", "0 9 * * 1-5", "30 2 1,15 * *", "0 0 * JAN,JUL MON", "5-55/10 0-23/2 * * 0", "0 12 * * 7", "  0 0 * * *  "]) {
      expect(cronReason(cron), cron).toBeNull();
    }
  });

  it("refuses_names_in_the_wrong_field_and_a_bare_L", () => {
    expect(cronReason("0 0 * * jan")).toMatch(/day of week/);
    expect(cronReason("mon * * * *")).toMatch(/minute/);
    expect(cronReason("0 0 * * L")).toMatch(/day of week/);
    expect(cronReason("0 0 L * *")).toMatch(/day of month/);
    expect(cronReason("0 0 * * 9L")).toMatch(/day of week/);
    expect(cronReason("0 0 * * L5")).toMatch(/day of week/);
  });

  it("accepts_names_in_their_own_field_and_nth_last_weekday", () => {
    for (const cron of ["0 0 * * MON-FRI", "0 0 1 JAN *", "0 0 * * 5L", "*/5 9-17 * * MON-FRI"]) {
      expect(cronReason(cron), cron).toBeNull();
    }
  });

  it("needs_five_fields", () => {
    expect(cronReason("")).toMatch(/Enter a cron/);
    expect(cronReason("* * * *")).toMatch(/5 fields/);
    expect(cronReason("0 * * * * *")).toMatch(/5 fields/);
  });

  it("refuses_out_of_range_numbers", () => {
    expect(cronReason("60 * * * *")).toMatch(/minute/);
    expect(cronReason("* 24 * * *")).toMatch(/hour/);
    expect(cronReason("* * 0 * *")).toMatch(/day of month/);
    expect(cronReason("* * 32 * *")).toMatch(/day of month/);
    expect(cronReason("* * * 13 *")).toMatch(/month/);
    expect(cronReason("* * * * 8")).toMatch(/day of week/);
    expect(cronReason("0-60 * * * *")).toMatch(/minute/);
    expect(cronReason("*/0 * * * *")).toMatch(/minute/);
  });

  it("refuses_stray_characters", () => {
    expect(cronReason("* * * * ;")).toMatch(/day of week/);
    expect(cronReason("@daily")).toMatch(/5 fields/);
  });
});

describe("timezoneReason", () => {
  it("accepts_iana_zones", () => {
    expect(timezoneReason("UTC")).toBeNull();
    expect(timezoneReason("Europe/London")).toBeNull();
    expect(timezoneReason("America/New_York")).toBeNull();
  });

  it("refuses_empty_and_unknown", () => {
    expect(timezoneReason("")).toMatch(/Enter a time zone/);
    expect(timezoneReason("Mars/Olympus")).toMatch(/isn't a time zone/);
  });
});

describe("scheduleFormReducer", () => {
  it("starts_blank_in_utc", () => {
    expect(initialScheduleForm()).toEqual({ editing: null, cron: "", timezone: "UTC" });
  });

  it("edit_loads_the_schedule_and_reset_clears_it", () => {
    let s = scheduleFormReducer(initialScheduleForm(), { type: "edit", schedule: { id: "s1", cron: "0 9 * * *", timezone: "Europe/Paris", enabled: true, next_run_at: null, last_error: null } });
    expect(s).toEqual({ editing: "s1", cron: "0 9 * * *", timezone: "Europe/Paris" });
    s = scheduleFormReducer(s, { type: "set", field: "cron", value: "0 10 * * *" });
    expect(s.cron).toBe("0 10 * * *");
    expect(scheduleFormReducer(s, { type: "reset" })).toEqual(initialScheduleForm());
  });

  it("form_reason_checks_both_fields", () => {
    expect(scheduleFormReason({ editing: null, cron: "*/5 * * * *", timezone: "UTC" })).toBeNull();
    expect(scheduleFormReason({ editing: null, cron: "", timezone: "UTC" })).toMatch(/cron/);
    expect(scheduleFormReason({ editing: null, cron: "* * * * *", timezone: "Nowhere" })).toMatch(/time zone/);
  });
});

describe("scheduleStatus", () => {
  const base = { id: "s1", cron: "0 9 * * *", timezone: "UTC", enabled: false, next_run_at: null };

  it("is_stopped_with_the_reason", () => {
    const reason = 'cron: not a 5-field cron (<b>&)';
    expect(scheduleStatus({ ...base, last_error: reason })).toEqual({ kind: "stopped", reason });
  });

  it("is_not_stopped_without_a_reason", () => {
    expect(scheduleStatus({ ...base, last_error: null })).toEqual({ kind: "off" });
    expect(scheduleStatus({ ...base, last_error: "" })).toEqual({ kind: "off" });
    expect(scheduleStatus(base as never)).toEqual({ kind: "off" });
    expect(scheduleStatus({ ...base, enabled: true, last_error: null })).toEqual({ kind: "on" });
  });
});
