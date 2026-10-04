// The type layer: the `Database` shape `chelabase gen types` emits, the runtime type map,
// and the typed builder interfaces. No runtime code lives here.

/** A JSON value (`json` and `jsonb` columns). */
export type Json = string | number | boolean | null | Json[] | { [k: string]: Json };

/**
 * An `int8` column, argument or return in a generated `Database`. The client resolves it:
 * `number` by default, `bigint` with `createClient({ bigint: true })`. Never both.
 */
export type Int8 = { readonly __chelabase: "int8" };

/** The Postgres types whose JSON numbers the client reads from their exact source text. */
export type PgExact = "int8" | "numeric";

/**
 * The runtime companion of a generated `Database` (`export const types = {…} as const
 * satisfies TypeMap`): which columns, arguments and returns are `int8` or `numeric`. An
 * array column (`int8[]`, `numeric[]`) is listed with its element's kind.
 */
export interface TypeMap {
  /**
   * The schema an unqualified name means. Without it, an unqualified name uses the only
   * schema in the map that has it; when none or several do, the result is read as if
   * there were no map (any integer outside the safe range throws).
   */
  defaultSchema?: string;
  schemas: Record<
    string,
    {
      /** table -> column -> kind (only `int8`/`numeric` columns). */
      tables: Record<string, Record<string, PgExact>>;
      /** A scalar return's kind, or a set-returning function's row columns. */
      functions: Record<string, { returns?: PgExact; columns?: Record<string, PgExact> }>;
    }
  >;
}

/** The `Database` shape that `chelabase gen types` emits. */
export interface DatabaseShape {
  defaultSchema: string;
  /** Where the project's app users table lives; `chelabase gen types` writes it when app users are enabled. */
  appUsers?: { schema: string; table: string };
  schemas: Record<
    string,
    {
      tables: Record<
        string,
        { Row: Record<string, unknown>; Insert: Record<string, unknown>; Update: Record<string, unknown> }
      >;
      functions: Record<string, { Args: Record<string, unknown>; Returns: unknown }>;
    }
  >;
}

/** The JSON tree the data API receives (`POST /data/v1/query`). */
export type QueryTree = { [key: string]: unknown };

// --- Int8 resolution -------------------------------------------------------------------

type Int8ish = Int8 | readonly Int8ish[];
type DirectInt8<T> = [Extract<T, Int8ish>] extends [never] ? false : true;
/** Whether `T` (a union) holds Int8 directly, in an array, or as an object's property. */
type MentionsInt8<T> = true extends (
  T extends Int8ish
    ? true
    : T extends readonly unknown[]
      ? false
      : T extends object
        ? DirectInt8<T[keyof T]>
        : false
)
  ? true
  : false;

type BigOrNumber<B extends boolean> = B extends true ? bigint : number;

/** `Int8` → `number` (B false) or `bigint` (B true), through arrays and row objects. */
export type Resolve<T, B extends boolean> = T extends Int8
  ? BigOrNumber<B>
  : T extends readonly (infer E)[]
    ? MentionsInt8<E> extends true
      ? Resolve<E, B>[]
      : T
    : T extends object
      ? DirectInt8<T[keyof T]> extends true
        ? { [K in keyof T]: Resolve<T[K], B> }
        : T
      : T;

export type ResolveRow<R, B extends boolean> = { [K in keyof R]: Resolve<R[K], B> };

type Simplify<T> = { [K in keyof T]: T[K] } & {};

// --- names -----------------------------------------------------------------------------

type Schemas<DB extends DatabaseShape> = DB["schemas"];
type SchemaKey<DB extends DatabaseShape> = keyof Schemas<DB> & string;
type DefaultSchema<DB extends DatabaseShape> = DB["defaultSchema"] & SchemaKey<DB>;

type Named<DB extends DatabaseShape, Kind extends "tables" | "functions"> =
  | (keyof Schemas<DB>[DefaultSchema<DB>][Kind] & string)
  | { [S in SchemaKey<DB>]: `${S}.${keyof Schemas<DB>[S][Kind] & string}` }[SchemaKey<DB>];

type Lookup<DB extends DatabaseShape, Kind extends "tables" | "functions", N> = N extends `${infer S}.${infer T}`
  ? S extends SchemaKey<DB>
    ? T extends keyof Schemas<DB>[S][Kind]
      ? Schemas<DB>[S][Kind][T]
      : never
    : never
  : N extends keyof Schemas<DB>[DefaultSchema<DB>][Kind]
    ? Schemas<DB>[DefaultSchema<DB>][Kind][N]
    : never;

/** A table or view name: `"t"` in the default schema, or `"schema.t"`. */
export type TableName<DB extends DatabaseShape> = Named<DB, "tables">;
/** A function name: `"f"` in the default schema, or `"schema.f"`. */
export type FunctionName<DB extends DatabaseShape> = Named<DB, "functions">;

type TableOf<DB extends DatabaseShape, N> = Lookup<DB, "tables", N>;
type FunctionOf<DB extends DatabaseShape, N> = Lookup<DB, "functions", N>;

// --- filters ---------------------------------------------------------------------------

type Present<V> = Exclude<V, null | undefined>;
type Combinator = "and" | "or" | "not";

/** The operators on one column. Several in one object are combined with `and`. */
export type Ops<V> = {
  eq?: Present<V>;
  neq?: Present<V>;
  lt?: Present<V>;
  lte?: Present<V>;
  gt?: Present<V>;
  gte?: Present<V>;
  /** `in: []` matches no rows. */
  in?: readonly Present<V>[];
  /** `null` on any column; `true`/`false` on boolean columns. */
  is?: null | (boolean extends Present<V> ? boolean : never);
  /** Text columns only. */
  like?: string extends Present<V> ? string : never;
  /** Text columns only. */
  ilike?: string extends Present<V> ? string : never;
};

/**
 * A filter object: each key is a column (a bare value means `eq`, `null` means `is null`,
 * `{ op: v }` uses the operator) or one of `and`, `or`, `not`; keys are combined with
 * `and`. A column literally named `and`, `or` or `not` is reached with a condition
 * object, `{ col: "and", eq: … }`, inside an `and`/`or` list or under `not`.
 */
export type Where<Row> = {
  [K in Exclude<keyof Row & string, Combinator>]?: Row[K] | Ops<Row[K]>;
} & {
  and?: readonly WhereItem<Row>[];
  or?: readonly WhereItem<Row>[];
  not?: WhereItem<Row>;
};

/** A condition object `{ col, <operator>: value }`: reaches any column, `and`/`or`/`not` included. */
export type Condition<Row> = { [K in keyof Row & string]: { col: K } & Ops<Row[K]> }[keyof Row & string];

export type WhereItem<Row> = Where<Row> | Condition<Row>;

// --- select ----------------------------------------------------------------------------

/** A column, or `{ col, as }` to rename it in the result. */
export type SelectItem<Row> = (keyof Row & string) | { readonly col: keyof Row & string; readonly as: string };

type ItemKey<I> = I extends string ? I : I extends { readonly as: infer A extends string } ? A : never;
type ItemCol<I> = I extends string ? I : I extends { readonly col: infer C extends string } ? C : never;
type Selected<Row, C extends readonly unknown[]> = {
  [I in C[number] as ItemKey<I>]: Row[ItemCol<I> & keyof Row];
};

export type Direction = "asc" | "desc";
export type Nulls = "first" | "last";

// --- builders --------------------------------------------------------------------------

/** Type-only brand: what `batch` accepts. No runtime value. */
declare const runnable: unique symbol;

/** One operation: awaitable (each await sends it once), or usable inside a batch. */
export interface Operation<Out> extends PromiseLike<Out> {
 
  /** The exact JSON tree this operation sends. */
  toTree(): QueryTree;
  catch<R = never>(onrejected?: ((reason: unknown) => R | PromiseLike<R>) | null): Promise<Out | R>;
  finally(onfinally?: (() => void) | null): Promise<Out>;
}

export interface SelectQuery<Row, Out> extends Operation<Out[]> {
  /** Narrows the result to these columns; `{ col, as }` renames one. */
  select<const C extends readonly [SelectItem<Row>, ...SelectItem<Row>[]]>(...cols: C): SelectQuery<Row, Simplify<Selected<Row, C>>>;
  /** Filters the rows; a second call is combined with `and`. */
  where(filter: Where<Row>): SelectQuery<Row, Out>;
  /** Adds a sort key; `nulls` defaults to Postgres's (`last` for asc, `first` for desc). */
  orderBy(col: keyof Row & string, dir?: Direction, nulls?: Nulls): SelectQuery<Row, Out>;
  /** At most `n` rows (the server also clamps to its `max_rows`). */
  limit(n: number): SelectQuery<Row, Out>;
  offset(n: number): SelectQuery<Row, Out>;
}

type Returned<Row, C extends readonly unknown[]> = Simplify<Pick<Row, C[number] & keyof Row>>[];

type Columns<Row> = readonly [keyof Row & string, ...(keyof Row & string)[]];

export interface InsertQuery<Row> extends Operation<[]> {
  returning(all: "*"): Operation<Row[]>;
  returning<const C extends Columns<Row>>(...cols: C): Operation<Returned<Row, C>>;
}

/**
 * An update or delete before `.where()` or `.all()`: not runnable yet. Awaiting it or
 * putting it in a batch is a compile error (the server refuses it too).
 */
export interface UnfilteredWrite<Row, Out> {
  /** Not a promise yet: call `.where()` or `.all()` first. */
  readonly then: (callWhereOrAllFirst: never) => never;
  /** The JSON tree so far (without a filter, the server refuses it). */
  toTree(): QueryTree;
  /** Filters the rows; a second call is combined with `and`. */
  where(filter: Where<Row>): WriteQuery<Row, Out>;
  /** Writes every row. */
  all(): WriteQuery<Row, Out>;
  returning(all: "*"): UnfilteredWrite<Row, Row[]>;
  returning<const C extends Columns<Row>>(...cols: C): UnfilteredWrite<Row, Returned<Row, C>>;
}

/** An update or delete with `.where()` or `.all()`: runnable. */
export interface WriteQuery<Row, Out> extends Operation<Out> {
  /** Filters the rows; a second call is combined with `and`. */
  where(filter: Where<Row>): WriteQuery<Row, Out>;
  all(): WriteQuery<Row, Out>;
  returning(all: "*"): WriteQuery<Row, Row[]>;
  returning<const C extends Columns<Row>>(...cols: C): WriteQuery<Row, Returned<Row, C>>;
}

/** `db.from(name)`: selects every column when awaited, or starts a write. */
export interface TableQuery<Row, Ins, Upd> extends SelectQuery<Row, Row> {
  /** One row or a list; all rows must have the same keys. */
  insert(rows: Ins | readonly Ins[]): InsertQuery<Row>;
  /** Needs `.where()` or `.all()` before it runs. */
  update(set: Upd): UnfilteredWrite<Row, []>;
  /** Needs `.where()` or `.all()` before it runs. */
  delete(): UnfilteredWrite<Row, []>;
}

type Table<DB extends DatabaseShape, N, B extends boolean> =
  TableOf<DB, N> extends { Row: infer R; Insert: infer I; Update: infer U }
    ? TableQuery<ResolveRow<R, B>, ResolveRow<I, B>, ResolveRow<U, B>>
    : never;

type CallArgs<A> = {} extends A ? [args?: A] : [args: A];
type Fn<DB extends DatabaseShape, N> = FunctionOf<DB, N>;

/** What a batch callback receives: the client without `batch` (batches don't nest). */
export interface Tx<DB extends DatabaseShape, B extends boolean> {
  from<N extends TableName<DB>>(name: N): Table<DB, N, B>;
  call<N extends FunctionName<DB>>(
    fn: N,
    ...args: CallArgs<ResolveRow<Fn<DB, N>["Args"], B>>
  ): Operation<Resolve<Fn<DB, N>["Returns"], B>>;
}

type Results<T extends readonly unknown[]> = { -readonly [I in keyof T]: T[I] extends PromiseLike<infer R> ? R : never };

// --- app users (db.auth) -----------------------------------------------------------------

/** Where the refresh token is kept: sync or async. The default is `localStorage`, else memory. */
export interface AuthStorage {
  get(key: string): string | null | Promise<string | null>;
  set(key: string, value: string): void | Promise<void>;
  remove(key: string): void | Promise<void>;
}

export interface AuthOptions {
  storage?: AuthStorage;
  /**
   * Refresh the access token 60 s before it expires. Default `true`. This only controls the
   * background timer; a request with an expiring token (under 5 s left) still refreshes first.
   */
  autoRefresh?: boolean;
}

export interface AppUser {
  id: string;
  email: string;
  email_verified: boolean;
  created_at: string;
}

export interface Session {
  access_token: string;
  token_type: "bearer";
  expires_in: number;
  refresh_token: string;
  user: AppUser;
}

export type AuthEvent = "signed_in" | "signed_out" | "refreshed";

/** `signUp`'s `data`: the app users table's insertable columns, minus the managed ones. */
export type SignUpData<DB extends DatabaseShape> = DB extends {
  appUsers: { schema: infer S extends string; table: infer T extends string };
}
  ? Omit<DB["schemas"][S]["tables"][T]["Insert"], "id" | "email" | "email_verified" | "created_at" | "deleted_at">
  : Record<string, Json>;

export interface Auth<DB extends DatabaseShape> {
  signUp(p: {
    email: string;
    password: string;
    data?: SignUpData<DB>;
  }): Promise<{ user: AppUser; session: Session | null; verificationRequired: boolean }>;
  signIn(p: { email: string; password: string }): Promise<Session>;
  signOut(): Promise<void>;
  refresh(): Promise<Session>;
  getUser(): Promise<AppUser>;
  verifyEmail(p: { email: string; code: string }): Promise<void>;
  resendCode(p: { email: string }): Promise<void>;
  requestPasswordReset(p: { email: string }): Promise<void>;
  confirmPasswordReset(p: { email: string; code: string; password: string }): Promise<void>;
  changeEmail(p: { email: string }): Promise<void>;
  changePassword(p: { currentPassword: string; password: string }): Promise<Session>;
  /** Subscribe to sign-in, sign-out and refresh; returns the unsubscribe function. */
  onChange(cb: (event: AuthEvent, user: AppUser | null) => void): () => void;
}

export interface Client<DB extends DatabaseShape, B extends boolean> extends Tx<DB, B> {
  /** App user sign-up, sign-in and session handling. Shared by `database()` siblings. */
  readonly auth: Auth<DB>;
  /**
   * Runs the operations in order in one transaction, all or nothing (at most 50), and
   * resolves to each one's result.
   */
  batch<const T extends readonly Operation<unknown>[] | []>(fn: (tx: Tx<DB, B>) => T): Operation<Results<T>>;
  /**
   * A client bound to another database of the branch: every request it sends carries
   * `database: name` (a batch carries it once, at its top level). Pass the generated
   * `Databases["name"]` as `DB2` and `types["name"]` as `types`; without a generated type
   * the client is untyped (in bigint mode `types` is always required: it decodes int8). The bigint mode is inherited.
   */
  database<DB2 extends DatabaseShape = DatabaseShape>(
    name: string,
    ...types: B extends true ? [types: TypeMap] : DatabaseShape extends DB2 ? [types?: TypeMap] : [types: TypeMap]
  ): Client<DB2, B>;
}
