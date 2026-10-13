//! Top-level OpenAPI document; the route modules contribute paths via
//! `OpenApiRouter`. `print-openapi` writes it to stdout.

use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            "caller_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "x-pothos-caller-secret",
                "Shared secret held by the product layer; POTHOS_CALLER_SECRET on the deployment",
            ))),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "pothos-heart API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Open-source matchmaking engine: explainable candidate ranking from companion-derived profiles."
    ),
    tags(
        (name = "health", description = "Liveness"),
        (name = "profiles", description = "Hard attributes pushed by the product layer"),
        (name = "match", description = "Ranked candidates")
    ),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;
