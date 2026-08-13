window.APP_CONFIG = Object.freeze({
    posts_domain: "https://e621.net",
    // Relative path `/api` works everywhere:
    //   - dev:  Trunk proxy forwards `/api/*` → `127.0.0.1:8080`
    //   - prod: nginx proxy `/api/` → backend upstream
    // Same-origin means `SameSite=Lax` cookies work without issue.
    backend_domain: "/api",
});