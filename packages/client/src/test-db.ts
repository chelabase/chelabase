// A hand-written `Database` and `types` in the shape `cheladb gen types` emits; shared by
// the tests. Not exported from the package.
import type { Int8, Json, TypeMap } from "./index.ts";

export type Database = {
  defaultSchema: "public";
  schemas: {
    public: {
      tables: {
        orders: {
          Row: {
            id: Int8;
            total: string;
            status: "paid" | "pending" | "shipped";
            customer_name: string | null;
            region: string;
            vip: boolean;
            qty: number;
            created_at: string;
            tags: string[];
            meta: Json;
            and: string | null;
          };
          Insert: {
            id?: Int8;
            total: string;
            status?: "paid" | "pending" | "shipped";
            customer_name?: string | null;
            region: string;
            vip?: boolean;
            qty?: number;
            created_at?: string;
            tags?: string[];
            meta?: Json;
            and?: string | null;
          };
          Update: {
            id?: Int8;
            total?: string;
            status?: "paid" | "pending" | "shipped";
            customer_name?: string | null;
            region?: string;
            vip?: boolean;
            qty?: number;
            created_at?: string;
            tags?: string[];
            meta?: Json;
            and?: string | null;
          };
        };
        events: {
          Row: { id: Int8; seq: Int8 | null; amount: string; ids: Int8[]; label: string };
          Insert: { id?: Int8; seq?: Int8 | null; amount: string; ids?: Int8[]; label: string };
          Update: { id?: Int8; seq?: Int8 | null; amount?: string; ids?: Int8[]; label?: string };
        };
      };
      functions: {
        add_points: { Args: { user_id: string; n?: number }; Returns: number };
        total_spent: { Args: { user_id: string }; Returns: string };
        next_id: { Args: Record<string, never>; Returns: Int8 };
        big_ids: { Args: { since?: Int8 }; Returns: Int8[] };
        top_events: { Args: { n?: number }; Returns: { id: Int8; amount: string; label: string }[] };
        latest_event: { Args: Record<string, never>; Returns: { id: Int8; amount: string; label: string } };
      };
    };
    billing: {
      tables: {
        invoices: {
          Row: { id: string; amount: string; paid: boolean };
          Insert: { id?: string; amount: string; paid?: boolean };
          Update: { id?: string; amount?: string; paid?: boolean };
        };
      };
      functions: {
        void_invoice: { Args: { id: string }; Returns: null };
      };
    };
  };
};

export const types = {
  defaultSchema: "public",
  schemas: {
    public: {
      tables: {
        orders: { id: "int8", total: "numeric" },
        events: { id: "int8", seq: "int8", amount: "numeric", ids: "int8" },
      },
      functions: {
        add_points: {},
        total_spent: { returns: "numeric" },
        next_id: { returns: "int8" },
        big_ids: { returns: "int8" },
        top_events: { columns: { id: "int8", amount: "numeric" } },
        latest_event: { columns: { id: "int8", amount: "numeric" } },
      },
    },
    billing: {
      tables: { invoices: { amount: "numeric" } },
      functions: {},
    },
  },
} as const satisfies TypeMap;
