//! Management service requests (opcodes 0x0100 and up).
//!
//! These need no ATTACH and leave the debug state alone.
use super::*;
use crate::login::{
    self,
    messages::{Command0, Command1, Sec2Payload, SessionData},
};
use crate::manage::proto::{
    AssetBeginRequest, AssetBeginResponse, DeviceInfo, JobCancelRequest, JobRunOnceResponse,
    JobSetRequest, JobStatus, LogAckRequest, LogReadRequest, LogReadResponse, NetConfig, NetStatus,
    SetTimeRequest, Trigger,
};
use crate::manage::{
    ASSET_BEGIN, ASSET_COMMIT, ASSET_WRITE, DEVICE_INFO, JOB_CANCEL, JOB_GET, JOB_RUN_ONCE,
    JOB_SET, LOG_ACK, LOG_READ, LOGIN, NET_CONFIG_GET, NET_CONFIG_SET, NET_STATUS, SET_TIME,
};
use prost::Message;

impl<T: Transport> Session<T> {
    pub(super) fn decode_response<M: Message + Default>(&mut self, bytes: &[u8]) -> Result<M> {
        match M::decode(bytes) {
            Ok(message) => Ok(message),
            Err(_) => self.wire_error(invalid("malformed management response")),
        }
    }

    /// Serial number, firmware version and storage use.
    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        let payload = self.request(DEVICE_INFO, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Starts or resumes uploading an asset.
    pub fn asset_begin(
        &mut self,
        name: &str,
        size: u32,
        sha256: &[u8; 32],
    ) -> Result<AssetBeginResponse> {
        let request = AssetBeginRequest {
            name: name.to_owned(),
            size,
            sha256: sha256.to_vec(),
        };
        let payload = self.request(ASSET_BEGIN, request.encode_to_vec())?;
        self.decode_response(&payload)
    }

    /// Sends one chunk of the asset being uploaded.
    pub fn asset_write(&mut self, offset: u32, data: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(data);
        self.empty_request(ASSET_WRITE, payload)
    }

    /// Checks the uploaded asset's hash and stores it.
    pub fn asset_commit(&mut self) -> Result<()> {
        self.empty_request(ASSET_COMMIT, Vec::new())
    }

    /// Activates the staged job (or only changes the trigger of the active one).
    pub fn job_set(&mut self, sha256: &[u8; 32], trigger: Trigger) -> Result<()> {
        let request = JobSetRequest {
            sha256: sha256.to_vec(),
            trigger: trigger as i32,
        };
        self.empty_request(JOB_SET, request.encode_to_vec())
    }

    /// Active job, run state and the last run's result.
    pub fn job_get(&mut self) -> Result<JobStatus> {
        let payload = self.request(JOB_GET, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Starts one run of the active job and returns its run ID.
    pub fn job_run_once(&mut self) -> Result<u32> {
        let payload = self.request(JOB_RUN_ONCE, Vec::new())?;
        let response: JobRunOnceResponse = self.decode_response(&payload)?;
        Ok(response.run_id)
    }

    /// Asks run `run_id` (0: any run) to stop.
    pub fn job_cancel(&mut self, run_id: u32) -> Result<()> {
        let request = JobCancelRequest { run_id };
        self.empty_request(JOB_CANCEL, request.encode_to_vec())
    }

    /// Sets the device clock for this boot; the device keeps the first time it gets.
    pub fn set_time(&mut self, utc_ms: u64) -> Result<()> {
        let request = SetTimeRequest { utc_ms };
        self.empty_request(SET_TIME, request.encode_to_vec())
    }

    /// Production log entries after `after_id`, as many as fit one frame.
    pub fn log_read(&mut self, after_id: u64) -> Result<LogReadResponse> {
        let request = LogReadRequest { after_id };
        let payload = self.request(LOG_READ, request.encode_to_vec())?;
        self.decode_response(&payload)
    }

    /// Marks log entries up to `up_to_id` collected; the device may then overwrite them.
    pub fn log_ack(&mut self, up_to_id: u64) -> Result<()> {
        let request = LogAckRequest { up_to_id };
        self.empty_request(LOG_ACK, request.encode_to_vec())
    }

    /// Stored network settings; the WiFi password is never returned.
    pub fn net_config_get(&mut self) -> Result<NetConfig> {
        let payload = self.request(NET_CONFIG_GET, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Stores network settings for the device's next restart. USB only.
    pub fn net_config_set(&mut self, config: &NetConfig) -> Result<()> {
        self.empty_request(NET_CONFIG_SET, config.encode_to_vec())
    }

    /// WiFi and server connection state.
    pub fn net_status(&mut self) -> Result<NetStatus> {
        let payload = self.request(NET_STATUS, Vec::new())?;
        self.decode_response(&payload)
    }

    /// Logs in with the device's password; needed before anything else over
    /// the network. Fails with STATUS_UNAUTHORIZED for a wrong password, and
    /// with a state error when the device cannot prove it knows the password.
    pub fn login(&mut self, password: &str) -> Result<()> {
        let client = login::Client::new(password)?;
        let start = Sec2Payload {
            msg: 0,
            sc0: Some(Command0 {
                client_username: login::USERNAME.as_bytes().to_vec(),
                client_pubkey: client.public_key().to_vec(),
            }),
            ..Default::default()
        };
        let reply = self.login_step(start)?;
        let Some(challenge) = reply.sr0.filter(|r| r.status == 0) else {
            return self.wire_error(invalid("malformed login response"));
        };
        let Some((proof, device_proof)) =
            client.proofs(&challenge.device_salt, &challenge.device_pubkey)
        else {
            return self.wire_error(invalid("invalid login challenge"));
        };

        let finish = Sec2Payload {
            msg: 2,
            sc1: Some(Command1 {
                client_proof: proof,
            }),
            ..Default::default()
        };
        let reply = self.login_step(finish)?;
        match reply.sr1 {
            Some(answer) if answer.status == 0 && answer.device_proof == device_proof => Ok(()),
            _ => Err(Error::State(
                "the Soul Injector could not prove it knows the password; it may not be yours",
            )),
        }
    }

    fn login_step(&mut self, payload: Sec2Payload) -> Result<Sec2Payload> {
        let request = SessionData {
            sec_ver: 2,
            sec2: Some(payload),
        };
        let reply = self.request(LOGIN, request.encode_to_vec())?;
        let reply: SessionData = self.decode_response(&reply)?;
        match reply.sec2 {
            Some(payload) => Ok(payload),
            None => self.wire_error(invalid("malformed login response")),
        }
    }
}
