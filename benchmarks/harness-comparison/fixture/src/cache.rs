pub fn invoice_cache_key(tenant: &str, invoice_id: &str) -> String {
    format!("northstar:{tenant}:{invoice_id}:v2")
}

pub const CACHE_TTL_SECONDS: u64 = 900;
pub const NEGATIVE_CACHE_TTL_SECONDS: u64 = 30;
