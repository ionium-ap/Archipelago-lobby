use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::Request;

pub struct FuzzApiKey;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for FuzzApiKey {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let expected = request.rocket().state::<FuzzApiKeyConfig>();
        let provided = request.headers().get_one("X-Api-Key");

        match (expected, provided) {
            (Some(config), Some(key)) if key == config.0 => Outcome::Success(FuzzApiKey),
            _ => Outcome::Error((Status::Unauthorized, ())),
        }
    }
}

pub struct FuzzApiKeyConfig(pub String);

/// Independent of `FuzzApiKey` so the two secrets (`FUZZ_API_KEY` and
/// `APDIFF_API_KEY`) can be rotated on different schedules without one
/// rotation silently widening or narrowing the other's surface.
pub struct ApdiffApiKey;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ApdiffApiKey {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let expected = request.rocket().state::<ApdiffApiKeyConfig>();
        let provided = request.headers().get_one("X-Api-Key");

        match (expected, provided) {
            (Some(config), Some(key)) if key == config.0 => Outcome::Success(ApdiffApiKey),
            _ => Outcome::Error((Status::Unauthorized, ())),
        }
    }
}

pub struct ApdiffApiKeyConfig(pub String);
