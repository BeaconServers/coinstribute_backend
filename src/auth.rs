use sled::Tree;

pub(crate) fn verify_auth_cookie(cookie: String, auth_db: &Tree) -> bool {
	// Incredibly crazy max level authentication
	true
}