//! Native Queue with durable receipts; Submit remains capability-gated.
//! The isolated fixture exercises the durable dispatch boundary without sockets.
use super::*;
pub(super) mod queue;
pub(super) mod session_status;
mod submit;
#[cfg_attr(not(test), allow(dead_code))]
const VERSION: &str = "1.4.0";
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Submit,
    Queue,
}
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    pub session_id: String,
    pub mode: Mode,
    pub text: String,
    pub expected_generation: String,
    pub expected_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_previous_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_stop_id: Option<String>,
    pub capability_id: String,
    pub created_at: i64,
    pub expires_at: i64,
}
impl Request {
    fn validate(&self) -> Result<()> {
        uuid(&self.request_id)?;
        uuid(&self.session_id)?;
        if let Some(id) = &self.expected_stop_id {
            uuid(id)?;
            if !self.resume {
                return Err(Fault("invalid_precondition"));
            }
        }
        if self.text.is_empty()
            || self.text.len() > 16384
            || self.expected_generation.len() != 64
            || !self
                .expected_generation
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || self.capability_id.is_empty()
            || self.capability_id.len() > 128
            || self
                .expected_turn_id
                .as_ref()
                .is_some_and(|t| t.is_empty() || t.len() > 256 || t.chars().any(char::is_control))
            || self.created_at < 0
            || self.expires_at <= self.created_at
            || self.expires_at - self.created_at > 3600
        {
            return Err(Fault("invalid_request"));
        }
        if self
            .expected_previous_turn_id
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(Fault("invalid_precondition"));
        }
        if self.expected_turn_id.is_some()
            && (self.resume || self.expected_previous_turn_id.is_some())
        {
            return Err(Fault("invalid_precondition"));
        }
        if self.mode != Mode::Submit
            && (self.expected_turn_id.is_some()
                || self.expected_previous_turn_id.is_some()
                || self.resume)
        {
            return Err(Fault("invalid_precondition"));
        }
        Ok(())
    }
    fn fingerprint(&self) -> Result<String> {
        // Typed struct serializes in fixed field order; null defaults normalize omitted fields.
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| Fault("invalid_request"))?)
        ))
    }
}
// Use the authoritative managed binding, process lifetime, catalog owner and safe
// native endpoint checks. No recipient policy or caller-supplied home/socket.
