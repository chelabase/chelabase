//! Studio's `studio.env` in the workbench: the build values its Vite server reads (the API URL
//! and Studio's host). Rendered by `workbench up` from the stack env.

pub fn render_studio_env(api_url: &str, studio_host: &str) -> String {
    format!(
        "# Written by `just workbench up`; regenerate instead of editing.\n\
VITE_API_URL={api_url}\nVITE_STUDIO_HOST={studio_host}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_env_has_every_key_studio_reads() {
        assert_eq!(
            render_studio_env("http://api.chelabase.localhost:8000", "studio.chelabase.localhost:8000"),
            "# Written by `just workbench up`; regenerate instead of editing.\n\
VITE_API_URL=http://api.chelabase.localhost:8000\nVITE_STUDIO_HOST=studio.chelabase.localhost:8000\n"
        );
    }
}
