use super::*;

pub(crate) fn transcript_head(transcript: &SharedTranscript) -> Result<u64, HostError> {
    let transcript = transcript
        .lock()
        .map_err(|_| HostError::PoisonedLock("transcript"))?;
    Ok(transcript
        .start
        .saturating_add(transcript.bytes.len() as u64))
}

pub(crate) fn transcript_snapshot_bytes(
    transcript: &SharedTranscript,
    max_bytes: usize,
) -> Result<Vec<u8>, HostError> {
    let transcript = transcript
        .lock()
        .map_err(|_| HostError::PoisonedLock("transcript"))?;
    let keep = transcript.bytes.len().min(max_bytes);
    let start_index = transcript.bytes.len().saturating_sub(keep);
    Ok(transcript.bytes[start_index..].to_vec())
}

pub(crate) fn transcript_bytes_since(
    transcript: &SharedTranscript,
    cursor: u64,
    max_bytes: usize,
) -> Result<(u64, Vec<u8>), HostError> {
    let transcript = transcript
        .lock()
        .map_err(|_| HostError::PoisonedLock("transcript"))?;
    let available_start = transcript.start;
    let available_end = transcript
        .start
        .saturating_add(transcript.bytes.len() as u64);
    let start = cursor.max(available_start).min(available_end);
    let offset = start.saturating_sub(available_start) as usize;
    let keep = transcript.bytes.len().saturating_sub(offset).min(max_bytes);
    let bytes = transcript.bytes[offset..offset + keep].to_vec();
    Ok((start.saturating_add(bytes.len() as u64), bytes))
}
