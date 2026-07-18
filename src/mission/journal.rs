use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const FRAME_MAGIC: &[u8; 8] = b"MSNJRN03";
pub(crate) const FRAME_VERSION: u16 = 2;
const FRAME_PREFIX_LEN: usize = 8 + 2 + 4 + 32 + 32;
pub(crate) const FRAME_HEADER_LEN: usize = FRAME_PREFIX_LEN + 32;
const RECORD_HASH_LEN: usize = 32;
pub(crate) const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub(crate) const MAX_JOURNAL_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_JOURNAL_FRAMES: u64 = 250_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RecordHash([u8; 32]);

impl RecordHash {
    pub(crate) const ZERO: Self = Self([0; 32]);

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct StateHash([u8; 32]);

impl StateHash {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct VerifiedFrame {
    pub(crate) payload: Vec<u8>,
    pub(crate) hash: RecordHash,
    pub(crate) previous_hash: RecordHash,
    pub(crate) state_hash: StateHash,
    pub(crate) sequence: u64,
    pub(crate) end_offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReplayCheckpoint {
    pub(crate) sequence: u64,
    pub(crate) end_offset: u64,
    pub(crate) record_hash: RecordHash,
    pub(crate) state_hash: StateHash,
}

impl ReplayCheckpoint {
    const ZERO: Self = Self {
        sequence: 0,
        end_offset: 0,
        record_hash: RecordHash::ZERO,
        state_hash: StateHash([0; 32]),
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReplayLimits {
    pub(crate) max_frames: u64,
    pub(crate) max_bytes: u64,
}

impl ReplayLimits {
    const PRODUCTION: Self = Self {
        max_frames: MAX_JOURNAL_FRAMES,
        max_bytes: MAX_JOURNAL_BYTES,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReplaySummary {
    pub(crate) frame_count: u64,
    pub(crate) final_sequence: u64,
    pub(crate) bytes_consumed: u64,
    pub(crate) last_hash: RecordHash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScanSummary {
    pub(crate) frame_count: u64,
    pub(crate) bytes_consumed: u64,
    pub(crate) last_hash: RecordHash,
    pub(crate) checkpoints: [Option<ReplayCheckpoint>; 2],
}

#[derive(Debug)]
pub(crate) enum ReplayError<E> {
    Journal(JournalError),
    Visitor(E),
}

impl<E> From<std::io::Error> for ReplayError<E> {
    fn from(error: std::io::Error) -> Self {
        Self::Journal(JournalError::Io(error))
    }
}

#[derive(Debug)]
pub(crate) struct FramedJournal {
    file: File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplayMode {
    Inspect,
    RepairFinalPartial,
}

impl FramedJournal {
    pub(crate) fn new(file: File) -> Self {
        Self { file }
    }

    pub(crate) fn current_len(&self) -> Result<u64, JournalError> {
        Ok(self.file.metadata()?.len())
    }

    #[allow(
        dead_code,
        reason = "generic replay visitors are retained for the staged closure pipeline"
    )]
    pub(crate) fn replay_with<E, F>(
        &mut self,
        mode: ReplayMode,
        visitor: F,
    ) -> Result<ReplaySummary, ReplayError<E>>
    where
        F: FnMut(VerifiedFrame) -> Result<(), E>,
    {
        self.replay_with_limits(mode, ReplayLimits::PRODUCTION, visitor)
    }

    #[allow(
        dead_code,
        reason = "bounded replay visitors are retained for the staged closure pipeline"
    )]
    pub(crate) fn replay_with_limits<E, F>(
        &mut self,
        mode: ReplayMode,
        limits: ReplayLimits,
        visitor: F,
    ) -> Result<ReplaySummary, ReplayError<E>>
    where
        F: FnMut(VerifiedFrame) -> Result<(), E>,
    {
        self.replay_range(mode, limits, ReplayCheckpoint::ZERO, visitor)
    }

    pub(crate) fn scan(
        &mut self,
        mode: ReplayMode,
        target_sequences: [Option<u64>; 2],
    ) -> Result<ScanSummary, JournalError> {
        let mut checkpoints = [None, None];
        for (index, target) in target_sequences.into_iter().enumerate() {
            if target == Some(0) {
                checkpoints[index] = Some(ReplayCheckpoint::ZERO);
            }
        }
        let replay = self.replay_range(
            mode,
            ReplayLimits::PRODUCTION,
            ReplayCheckpoint::ZERO,
            |frame| {
                for (index, target) in target_sequences.into_iter().enumerate() {
                    if target == Some(frame.sequence) {
                        checkpoints[index] = Some(ReplayCheckpoint {
                            sequence: frame.sequence,
                            end_offset: frame.end_offset,
                            record_hash: frame.hash,
                            state_hash: frame.state_hash,
                        });
                    }
                }
                Ok::<_, std::convert::Infallible>(())
            },
        );
        let summary = match replay {
            Ok(summary) => summary,
            Err(ReplayError::Journal(error)) => return Err(error),
            Err(ReplayError::Visitor(never)) => match never {},
        };
        Ok(ScanSummary {
            frame_count: summary.final_sequence,
            bytes_consumed: summary.bytes_consumed,
            last_hash: summary.last_hash,
            checkpoints,
        })
    }

    pub(crate) fn replay_from<E, F>(
        &mut self,
        checkpoint: ReplayCheckpoint,
        visitor: F,
    ) -> Result<ReplaySummary, ReplayError<E>>
    where
        F: FnMut(VerifiedFrame) -> Result<(), E>,
    {
        self.replay_range(
            ReplayMode::Inspect,
            ReplayLimits::PRODUCTION,
            checkpoint,
            visitor,
        )
    }

    fn replay_range<E, F>(
        &mut self,
        mode: ReplayMode,
        limits: ReplayLimits,
        start: ReplayCheckpoint,
        mut visitor: F,
    ) -> Result<ReplaySummary, ReplayError<E>>
    where
        F: FnMut(VerifiedFrame) -> Result<(), E>,
    {
        let journal_bytes = self
            .file
            .metadata()
            .map_err(JournalError::from)
            .map_err(ReplayError::Journal)?
            .len();
        if journal_bytes > limits.max_bytes {
            return Err(ReplayError::Journal(JournalError::JournalTooLarge {
                bytes: journal_bytes,
                limit: limits.max_bytes,
            }));
        }
        if start.end_offset > journal_bytes {
            return Err(ReplayError::Journal(JournalError::InvalidReplayOffset {
                offset: start.end_offset,
            }));
        }
        self.file.seek(SeekFrom::Start(start.end_offset))?;
        let mut frame_count = 0_u64;
        let mut sequence = start.sequence;
        let mut last_hash = start.record_hash;

        loop {
            let frame_start = self.file.stream_position()?;
            let mut header = [0_u8; FRAME_HEADER_LEN];
            let header_bytes = read_until_full_or_eof(&mut self.file, &mut header)?;
            if header_bytes == 0 {
                break;
            }
            if header_bytes < FRAME_HEADER_LEN {
                if !FRAME_MAGIC.starts_with(&header[..header_bytes.min(FRAME_MAGIC.len())]) {
                    return Err(ReplayError::Journal(JournalError::InvalidMagic {
                        offset: frame_start,
                    }));
                }
                self.handle_partial_tail(mode, frame_start)
                    .map_err(ReplayError::Journal)?;
                break;
            }

            if &header[..8] != FRAME_MAGIC {
                return Err(ReplayError::Journal(JournalError::InvalidMagic {
                    offset: frame_start,
                }));
            }
            let version = u16::from_be_bytes([header[8], header[9]]);
            if version != FRAME_VERSION {
                return Err(ReplayError::Journal(JournalError::UnsupportedFrameVersion(
                    version,
                )));
            }
            let payload_length =
                u32::from_be_bytes([header[10], header[11], header[12], header[13]]) as usize;
            if payload_length > MAX_PAYLOAD_BYTES {
                return Err(ReplayError::Journal(JournalError::PayloadTooLarge(
                    payload_length,
                )));
            }

            let previous_hash = record_hash_from_slice(&header[14..46]);
            let state_hash = state_hash_from_slice(&header[46..78]);
            let expected_header_hash = hash_header(&header[..FRAME_PREFIX_LEN]);
            if header[FRAME_PREFIX_LEN..FRAME_HEADER_LEN] != expected_header_hash.0 {
                return Err(ReplayError::Journal(JournalError::HeaderHashMismatch {
                    offset: frame_start,
                }));
            }

            let mut payload = vec![0_u8; payload_length];
            if read_until_full_or_eof(&mut self.file, &mut payload)? < payload.len() {
                self.handle_partial_tail(mode, frame_start)
                    .map_err(ReplayError::Journal)?;
                break;
            }
            let mut stored_hash = [0_u8; RECORD_HASH_LEN];
            if read_until_full_or_eof(&mut self.file, &mut stored_hash)? < stored_hash.len() {
                self.handle_partial_tail(mode, frame_start)
                    .map_err(ReplayError::Journal)?;
                break;
            }
            let expected_record_hash = hash_record(&header, &payload);
            if stored_hash != expected_record_hash.0 {
                return Err(ReplayError::Journal(JournalError::RecordHashMismatch {
                    offset: frame_start,
                }));
            }
            frame_count = frame_count.checked_add(1).ok_or_else(|| {
                ReplayError::Journal(JournalError::FrameLimitExceeded {
                    limit: limits.max_frames,
                })
            })?;
            sequence = sequence.checked_add(1).ok_or_else(|| {
                ReplayError::Journal(JournalError::FrameLimitExceeded {
                    limit: limits.max_frames,
                })
            })?;
            if sequence > limits.max_frames {
                return Err(ReplayError::Journal(JournalError::FrameLimitExceeded {
                    limit: limits.max_frames,
                }));
            }
            if previous_hash != last_hash {
                return Err(ReplayError::Journal(JournalError::BrokenRecordChain {
                    sequence,
                }));
            }
            last_hash = expected_record_hash;
            let end_offset = self.file.stream_position()?;
            visitor(VerifiedFrame {
                payload,
                hash: expected_record_hash,
                previous_hash,
                state_hash,
                sequence,
                end_offset,
            })
            .map_err(ReplayError::Visitor)?;
        }

        self.file.seek(SeekFrom::End(0))?;
        Ok(ReplaySummary {
            frame_count,
            final_sequence: sequence,
            bytes_consumed: self.file.stream_position()?,
            last_hash,
        })
    }

    pub(crate) fn append(
        &mut self,
        payload: &[u8],
        previous_hash: RecordHash,
        state_hash: StateHash,
    ) -> Result<RecordHash, JournalError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(JournalError::PayloadTooLarge(payload.len()));
        }
        let payload_length = u32::try_from(payload.len())
            .map_err(|_| JournalError::PayloadTooLarge(payload.len()))?;
        let next_length = self
            .file
            .metadata()?
            .len()
            .checked_add(FRAME_HEADER_LEN as u64)
            .and_then(|length| length.checked_add(payload.len() as u64))
            .and_then(|length| length.checked_add(RECORD_HASH_LEN as u64))
            .ok_or(JournalError::JournalTooLarge {
                bytes: u64::MAX,
                limit: MAX_JOURNAL_BYTES,
            })?;
        if next_length > MAX_JOURNAL_BYTES {
            return Err(JournalError::JournalTooLarge {
                bytes: next_length,
                limit: MAX_JOURNAL_BYTES,
            });
        }
        let mut header = [0_u8; FRAME_HEADER_LEN];
        header[..8].copy_from_slice(FRAME_MAGIC);
        header[8..10].copy_from_slice(&FRAME_VERSION.to_be_bytes());
        header[10..14].copy_from_slice(&payload_length.to_be_bytes());
        header[14..46].copy_from_slice(previous_hash.as_bytes());
        header[46..78].copy_from_slice(state_hash.as_bytes());
        let header_hash = hash_header(&header[..FRAME_PREFIX_LEN]);
        header[FRAME_PREFIX_LEN..].copy_from_slice(&header_hash.0);
        let record_hash = hash_record(&header, payload);

        self.file.write_all(&header)?;
        self.file.write_all(payload)?;
        self.file.write_all(&record_hash.0)?;
        self.file.sync_data()?;
        Ok(record_hash)
    }

    fn repair_partial_tail(&mut self, frame_start: u64) -> Result<(), JournalError> {
        self.file.set_len(frame_start)?;
        self.file.sync_data()?;
        self.file.seek(SeekFrom::Start(frame_start))?;
        Ok(())
    }

    fn handle_partial_tail(
        &mut self,
        mode: ReplayMode,
        frame_start: u64,
    ) -> Result<(), JournalError> {
        match mode {
            ReplayMode::Inspect => Err(JournalError::PartialTail {
                offset: frame_start,
            }),
            ReplayMode::RepairFinalPartial => self.repair_partial_tail(frame_start),
        }
    }
}

fn record_hash_from_slice(bytes: &[u8]) -> RecordHash {
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(bytes);
    RecordHash(hash)
}

fn state_hash_from_slice(bytes: &[u8]) -> StateHash {
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(bytes);
    StateHash(hash)
}

fn read_until_full_or_eof(file: &mut File, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..])? {
            0 => break,
            count => read += count,
        }
    }
    Ok(read)
}

fn hash_header(header_prefix: &[u8]) -> RecordHash {
    let mut hasher = Sha256::new();
    hasher.update(b"mission-journal-header-v1\0");
    hasher.update(header_prefix);
    RecordHash(hasher.finalize().into())
}

fn hash_record(header: &[u8], payload: &[u8]) -> RecordHash {
    let mut hasher = Sha256::new();
    hasher.update(b"mission-journal-record-v1\0");
    hasher.update(header);
    hasher.update(payload);
    RecordHash(hasher.finalize().into())
}

#[derive(Debug, Error)]
pub(crate) enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal frame at byte {offset} has invalid magic")]
    InvalidMagic { offset: u64 },
    #[error("journal frame version {0} is not supported")]
    UnsupportedFrameVersion(u16),
    #[error("journal frame payload is too large: {0} bytes")]
    PayloadTooLarge(usize),
    #[error("journal frame header hash mismatch at byte {offset}")]
    HeaderHashMismatch { offset: u64 },
    #[error("journal frame record hash mismatch at byte {offset}")]
    RecordHashMismatch { offset: u64 },
    #[error("journal has a partial final frame at byte {offset}")]
    PartialTail { offset: u64 },
    #[error("journal is too large: {bytes} bytes exceeds the {limit} byte limit")]
    JournalTooLarge { bytes: u64, limit: u64 },
    #[error("journal frame count exceeds the {limit} frame limit")]
    FrameLimitExceeded { limit: u64 },
    #[error("journal record chain is broken at sequence {sequence}")]
    BrokenRecordChain { sequence: u64 },
    #[error("journal replay offset {offset} is outside the journal")]
    InvalidReplayOffset { offset: u64 },
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, fs::OpenOptions};

    use super::*;

    fn journal() -> (tempfile::TempDir, FramedJournal) {
        let directory = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(directory.path().join("journal.bin"))
            .unwrap();
        (directory, FramedJournal::new(file))
    }

    fn append(
        journal: &mut FramedJournal,
        previous_hash: RecordHash,
        state_byte: u8,
        payload: &[u8],
    ) -> RecordHash {
        journal
            .append(
                payload,
                previous_hash,
                StateHash::from_bytes([state_byte; 32]),
            )
            .unwrap()
    }

    #[test]
    fn replay_streams_frames_in_order() {
        let (_directory, mut journal) = journal();
        let first = append(&mut journal, RecordHash::ZERO, 1, b"first");
        append(&mut journal, first, 2, b"second");

        let mut payloads = Vec::new();
        let summary = journal
            .replay_with(ReplayMode::Inspect, |frame| {
                payloads.push(frame.payload);
                Ok::<_, Infallible>(())
            })
            .unwrap();

        assert_eq!(payloads, [b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(summary.frame_count, 2);
    }

    #[test]
    fn replay_rejects_a_frame_count_over_the_explicit_limit() {
        let (_directory, mut journal) = journal();
        let first = append(&mut journal, RecordHash::ZERO, 1, b"first");
        append(&mut journal, first, 2, b"second");

        let error = journal
            .replay_with_limits(
                ReplayMode::Inspect,
                ReplayLimits {
                    max_frames: 1,
                    max_bytes: u64::MAX,
                },
                |_| Ok::<_, Infallible>(()),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ReplayError::Journal(JournalError::FrameLimitExceeded { limit: 1 })
        ));
    }

    #[test]
    fn replay_rejects_a_journal_over_the_explicit_byte_limit() {
        let (_directory, mut journal) = journal();
        append(&mut journal, RecordHash::ZERO, 1, b"payload");

        let error = journal
            .replay_with_limits(
                ReplayMode::Inspect,
                ReplayLimits {
                    max_frames: u64::MAX,
                    max_bytes: 1,
                },
                |_| Ok::<_, Infallible>(()),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ReplayError::Journal(JournalError::JournalTooLarge { limit: 1, .. })
        ));
    }

    #[test]
    fn authenticated_scan_then_replay_from_checkpoint_visits_only_the_tail() {
        let (_directory, mut journal) = journal();
        let first = append(&mut journal, RecordHash::ZERO, 1, b"prefix");
        append(&mut journal, first, 2, b"tail");

        let scan = journal.scan(ReplayMode::Inspect, [Some(1), None]).unwrap();
        let checkpoint = scan.checkpoints[0].unwrap();
        assert_eq!(checkpoint.sequence, 1);
        assert_eq!(checkpoint.record_hash, first);
        assert_eq!(checkpoint.state_hash, StateHash::from_bytes([1; 32]));

        let mut payloads = Vec::new();
        journal
            .replay_from(checkpoint, |frame| {
                payloads.push(frame.payload);
                Ok::<_, Infallible>(())
            })
            .unwrap();

        assert_eq!(payloads, [b"tail".to_vec()]);
    }
}
