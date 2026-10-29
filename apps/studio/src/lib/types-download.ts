import { fetchDataApiTypes, type GetDataApiTypesParams } from "@chelabase/management-client";

/** Which database(s) the types describe: tagged, so no database name can read as another kind. */
export type TypesDatabase = { kind: "main" } | { kind: "named"; name: string } | { kind: "all" };

export interface TypesChoice {
  /** The secret key role whose view the types describe. */
  role: string;
  /** The main database, one named database, or every served database (`all_databases=true`). */
  database: TypesDatabase;
}

/** The download's query and file name. */
export function typesRequest(choice: TypesChoice): { params: GetDataApiTypesParams; filename: string } {
  const params: GetDataApiTypesParams = { role: choice.role.trim() };
  if (choice.database.kind === "all") params.all_databases = true;
  else if (choice.database.kind === "named") params.database = choice.database.name;
  return { params, filename: "db.ts" };
}

/** Saves `text` as a file through a temporary link (the module never enters the query cache). */
export function saveFile(text: string, filename: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: "text/typescript;charset=utf-8" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

/** Fetches the branch's `db.ts` for the choice and saves it; throws the API error on a refusal. */
export async function downloadTypes(
  databaseId: string,
  branchId: string,
  choice: TypesChoice,
  save: (text: string, filename: string) => void = saveFile,
): Promise<void> {
  if (choice.role.trim() === "") throw new Error("Enter a role.");
  const { params, filename } = typesRequest(choice);
  save(await fetchDataApiTypes(databaseId, branchId, params), filename);
}
