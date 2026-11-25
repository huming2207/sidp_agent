//! Agent service requests (opcodes 0x0300 and up).
//!
//! An agent session holds the target the way an ATTACH does, so the two
//! exclude each other; see [`crate::agent`].
use super::*;
use crate::agent::{
    self, AGENT_CLOSE, AGENT_INFO, AGENT_OPEN, CAPTURE_READ, EVENT_READ, INCIDENT_DELETE,
    INCIDENT_LIST, INCIDENT_READ, SCRIPT_EVAL, SCRIPT_RUN, SCRIPT_STOP, SCRIPT_UPLOAD, TRAP_ARM,
    TRAP_DISARM, TRAP_STATUS,
    proto::{
        ArmRequest, CaptureReadRequest, CaptureReadResponse, EvalRequest, EventReadRequest,
        EventReadResponse, IncidentDeleteRequest, IncidentListRequest, IncidentListResponse,
        IncidentReadRequest, IncidentReadResponse, OpenRequest, RunRequest, ScriptResult,
        SessionInfo, TrapConfig, TrapStatus, UploadRequest,
    },
};
use prost::Message as _;

/// How long the device waits for a script when the request says 0.
const DEFAULT_SCRIPT_WAIT: Duration = Duration::from_secs(5);
/// Longest a request may ask the device to wait for a script.
pub const MAX_SCRIPT_WAIT: Duration = Duration::from_secs(60);
/// On top of the script's wait: a target operation the script started (a
/// flash erase, say) finishes before the device answers.
const SCRIPT_REPLY_SLACK: Duration = Duration::from_secs(30);

impl<T: Transport> Session<T> {
    /// Opens an agent session: the device claims the target and starts serving scripts.
    pub fn agent_open(&mut self, request: &OpenRequest) -> Result<SessionInfo> {
        if self.info.is_some() {
            return Err(Error::State("attached for debugging; detach first"));
        }
        self.agent_notice = None;
        let payload = self.request(AGENT_OPEN, request.encode_to_vec())?;
        self.decode_response(&payload)
    }

    /// Ends the agent session, leaving the target as OpenRequest.session_end said.
    pub fn agent_close(&mut self) -> Result<()> {
        self.empty_request(AGENT_CLOSE, Vec::new())
    }

    /// The open session's settings and newest event.
    pub fn agent_info(&mut self) -> Result<SessionInfo> {
        let payload = self.request(AGENT_INFO, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Replaces the session's runtime with `container` (one ES module) and
    /// waits up to `wait` for its top level. Modules too big for one frame
    /// are uploaded first.
    pub fn script_run(&mut self, container: &[u8], wait: Duration) -> Result<ScriptResult> {
        let wait = wait.min(MAX_SCRIPT_WAIT);
        let mut request = RunRequest {
            timeout_ms: wait.as_millis() as u32,
            ..Default::default()
        };
        if container.len() <= agent::MAX_INLINE {
            request.bytecode = container.to_vec();
        } else {
            self.script_upload(container)?;
            request.staged = true;
        }
        self.script_request(SCRIPT_RUN, request.encode_to_vec(), wait)
    }

    fn script_upload(&mut self, container: &[u8]) -> Result<()> {
        let total = u32::try_from(container.len()).map_err(|_| Error::State("script too big"))?;
        for (i, chunk) in container.chunks(agent::MAX_INLINE).enumerate() {
            let request = UploadRequest {
                offset: (i * agent::MAX_INLINE) as u32,
                total,
                data: chunk.to_vec(),
            };
            self.empty_request(SCRIPT_UPLOAD, request.encode_to_vec())?;
        }
        Ok(())
    }

    /// Runs `container` (an eval script, `sidp_qjsc::compile_eval`) in the
    /// session's runtime, starting one when none runs, and waits up to `wait`
    /// for its value.
    pub fn script_eval(&mut self, container: &[u8], wait: Duration) -> Result<ScriptResult> {
        if container.len() > agent::MAX_INLINE {
            return Err(Error::State(
                "eval script too big for one request; use a run",
            ));
        }
        let wait = wait.min(MAX_SCRIPT_WAIT);
        let request = EvalRequest {
            bytecode: container.to_vec(),
            timeout_ms: wait.as_millis() as u32,
        };
        self.script_request(SCRIPT_EVAL, request.encode_to_vec(), wait)
    }

    fn script_request(
        &mut self,
        opcode: u16,
        payload: Vec<u8>,
        wait: Duration,
    ) -> Result<ScriptResult> {
        let wait = if wait.is_zero() {
            DEFAULT_SCRIPT_WAIT
        } else {
            wait
        };
        let budget = wait + SCRIPT_REPLY_SLACK.max(self.timeout);
        let payload = self.request_within(opcode, payload, budget)?;
        self.decode_response(&payload)
    }

    /// Stops the session's runtime; its watches go with it.
    pub fn script_stop(&mut self) -> Result<()> {
        self.empty_request(SCRIPT_STOP, Vec::new())
    }

    /// Events after `after_seq`, oldest first, a few at a time.
    pub fn event_read(&mut self, after_seq: u32) -> Result<EventReadResponse> {
        let request = EventReadRequest { after_seq };
        let payload = self.request(EVENT_READ, request.encode_to_vec())?;
        self.decode_response(&payload)
    }

    /// A whole capture.
    pub fn capture_read(&mut self, capture_id: u32) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let request = CaptureReadRequest {
                capture_id,
                offset: data.len() as u32,
            };
            let payload = self.request(CAPTURE_READ, request.encode_to_vec())?;
            let part: CaptureReadResponse = self.decode_response(&payload)?;
            let total = part.total as usize;
            if part.data.is_empty() && data.len() < total {
                return self.wire_error(invalid("CAPTURE_READ made no progress"));
            }
            data.extend(part.data);
            if data.len() > total {
                return self.wire_error(invalid("CAPTURE_READ past the end"));
            }
            if data.len() == total {
                return Ok(data);
            }
        }
    }

    /// Arms `container` (one ES module) as the device's trap, replacing the
    /// armed one, and waits up to `wait` for its top level. Whether it was
    /// armed is in the status's `config` and `first_result`.
    pub fn trap_arm(
        &mut self,
        config: TrapConfig,
        container: &[u8],
        wait: Duration,
    ) -> Result<TrapStatus> {
        let wait = wait.min(MAX_SCRIPT_WAIT);
        let mut request = ArmRequest {
            config: Some(config),
            timeout_ms: wait.as_millis() as u32,
            ..Default::default()
        };
        if container.len() <= agent::MAX_INLINE {
            request.bytecode = container.to_vec();
        } else {
            self.script_upload(container)?;
            request.staged = true;
        }
        let wait = if wait.is_zero() {
            DEFAULT_SCRIPT_WAIT
        } else {
            wait
        };
        let budget = wait + SCRIPT_REPLY_SLACK.max(self.timeout);
        let payload = self.request_within(TRAP_ARM, request.encode_to_vec(), budget)?;
        self.decode_response(&payload)
    }

    /// Disarms the trap; the target is left as it is.
    pub fn trap_disarm(&mut self) -> Result<()> {
        self.empty_request(TRAP_DISARM, Vec::new())
    }

    /// The trap's state and the device's incident count.
    pub fn trap_status(&mut self) -> Result<TrapStatus> {
        let payload = self.request(TRAP_STATUS, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Every incident after `after_id`, oldest first.
    pub fn incident_list(&mut self, mut after_id: u32) -> Result<Vec<agent::IncidentInfo>> {
        let mut all = Vec::new();
        loop {
            let request = IncidentListRequest { after_id };
            let payload = self.request(INCIDENT_LIST, request.encode_to_vec())?;
            let part: IncidentListResponse = self.decode_response(&payload)?;
            let Some(last) = part.incidents.last().map(|i| i.id) else {
                return Ok(all);
            };
            if last <= after_id {
                return self.wire_error(invalid("INCIDENT_LIST went backwards"));
            }
            after_id = last;
            all.extend(part.incidents);
            if !part.more {
                return Ok(all);
            }
        }
    }

    /// A whole incident file.
    pub fn incident_read(&mut self, id: u32) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let request = IncidentReadRequest {
                id,
                offset: data.len() as u32,
            };
            let payload = self.request(INCIDENT_READ, request.encode_to_vec())?;
            let part: IncidentReadResponse = self.decode_response(&payload)?;
            let total = part.total as usize;
            if part.data.is_empty() && data.len() < total {
                return self.wire_error(invalid("INCIDENT_READ made no progress"));
            }
            data.extend(part.data);
            if data.len() > total {
                return self.wire_error(invalid("INCIDENT_READ past the end"));
            }
            if data.len() == total {
                return Ok(data);
            }
        }
    }

    /// Deletes incident `id`, or all of them for 0.
    pub fn incident_delete(&mut self, id: u32) -> Result<()> {
        let request = IncidentDeleteRequest { id };
        self.empty_request(INCIDENT_DELETE, request.encode_to_vec())
    }

    /// The newest event sequence number an AGENT_EVENT notice has announced
    /// since the last call; notices are expendable, so this is only a hint.
    pub fn take_agent_notice(&mut self) -> Option<u32> {
        self.agent_notice.take()
    }
}
