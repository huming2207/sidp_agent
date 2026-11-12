//! Management service requests (opcodes 0x0100 and up).
//!
//! These need no ATTACH and leave the debug state alone.
use super::*;
use crate::manage::proto::{
    AssetBeginRequest, AssetBeginResponse, DeviceInfo, JobCancelRequest, JobRunOnceResponse,
    JobSetRequest, JobStatus, LogAckRequest, LogReadRequest, LogReadResponse, SetTimeRequest,
    Trigger,
};
use crate::manage::{
    ASSET_BEGIN, ASSET_COMMIT, ASSET_WRITE, DEVICE_INFO, JOB_CANCEL, JOB_GET, JOB_RUN_ONCE,
    JOB_SET, LOG_ACK, LOG_READ, SET_TIME,
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
}
