// Vite's `?url` imports (apps also get this from vite/client; the package has no Vite types).
declare module "*.woff2?url" {
  const url: string;
  export default url;
}
