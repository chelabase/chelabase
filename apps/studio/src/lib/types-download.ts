import { fetchDataApiTypes, type GetDataApiTypesParams } from "@chelabase/management-client";

/** The database choice that downloads every served database (`all_databases=true`). */
export const ALL_DATABASES = "*all*";

export interface TypesChoice {
  /** The secret key role whose view the types describe. */
  role: string;
  /** `null`: the main database; a name; or {@link ALL_DATABASES}. */
  database: string | null;
}

/** The download's query and file name. */
export function typesRequest(choice: TypesChoice): { params: GetDataApiTypesParams; filename: string } {
  const params: GetDataApiTypesParams = { role: choice.role.trim() };
  if (choice.database === ALL_DATABASES) params.all_databases = true;
  else if (choice.database !== null) params.database = choice.database;
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
