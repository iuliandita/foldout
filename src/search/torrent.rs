//! BitTorrent v1 protocol identity, never a security fingerprint.
use bendy::decoding::{Decoder, Object};
use sha1::{Digest, Sha1};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TorrentError {
    #[error("invalid or noncanonical v1 torrent metadata")]
    Invalid,
    #[error("v2 and hybrid torrents are not supported")]
    UnsupportedV2,
}
impl From<bendy::decoding::Error> for TorrentError {
    fn from(_: bendy::decoding::Error) -> Self {
        Self::Invalid
    }
}

pub fn v1_infohash(bytes: &[u8]) -> Result<String, TorrentError> {
    if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
        return Err(TorrentError::Invalid);
    }
    let mut decoder = Decoder::new(bytes).with_max_depth(16);
    let mut root = decoder
        .next_object()?
        .ok_or(TorrentError::Invalid)?
        .try_into_dictionary()?;
    let mut info = None;
    while let Some((key, value)) = root.next_pair()? {
        match key {
            b"info" => info = Some(value.try_into_dictionary()?.into_raw()?),
            b"piece layers" => return Err(TorrentError::UnsupportedV2),
            _ => drop(value),
        }
    }
    drop(root);
    if decoder.next_object()?.is_some() {
        return Err(TorrentError::Invalid);
    }
    let info = info.ok_or(TorrentError::Invalid)?;
    validate_info(info)?;
    // into_raw returns the original dictionary including its delimiters. No re-encoding.
    Ok(Sha1::digest(info)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn validate_info(bytes: &[u8]) -> Result<(), TorrentError> {
    let mut decoder = Decoder::new(bytes).with_max_depth(16);
    let mut info = decoder
        .next_object()?
        .ok_or(TorrentError::Invalid)?
        .try_into_dictionary()?;
    let (mut name, mut piece_length, mut pieces, mut length, mut files) =
        (false, None, None, None, None);
    while let Some((key, value)) = info.next_pair()? {
        match key {
            b"meta version" | b"file tree" => return Err(TorrentError::UnsupportedV2),
            b"name" => {
                component(value.try_into_bytes()?)?;
                name = true;
            }
            b"piece length" => piece_length = Some(number(value)?),
            b"pieces" => pieces = Some(value.try_into_bytes()?.len() as u64),
            b"length" => length = Some(number(value)?),
            b"files" => {
                let mut list = value.try_into_list()?;
                let mut total = 0u64;
                let mut count = 0usize;
                let mut paths = std::collections::HashSet::new();
                while let Some(value) = list.next_object()? {
                    count += 1;
                    if count > 10000 {
                        return Err(TorrentError::Invalid);
                    }
                    let mut file = value.try_into_dictionary()?;
                    let (mut size, mut path) = (None, None);
                    while let Some((key, value)) = file.next_pair()? {
                        match key {
                            b"length" => size = Some(number(value)?),
                            b"path" => {
                                let mut parts = value.try_into_list()?;
                                let mut names = Vec::new();
                                while let Some(part) = parts.next_object()? {
                                    let part = part.try_into_bytes()?;
                                    component(part)?;
                                    names.push(part.to_vec());
                                    if names.len() > 32 {
                                        return Err(TorrentError::Invalid);
                                    }
                                }
                                if names.is_empty() {
                                    return Err(TorrentError::Invalid);
                                }
                                path = Some(names);
                            }
                            b"symlink path" => return Err(TorrentError::Invalid),
                            b"attr" => {
                                if value.try_into_bytes()?.contains(&b'l') {
                                    return Err(TorrentError::Invalid);
                                }
                            }
                            _ => drop(value),
                        }
                    }
                    if !paths.insert(path.ok_or(TorrentError::Invalid)?) {
                        return Err(TorrentError::Invalid);
                    }
                    total = total
                        .checked_add(size.ok_or(TorrentError::Invalid)?)
                        .ok_or(TorrentError::Invalid)?;
                }
                if count == 0 {
                    return Err(TorrentError::Invalid);
                }
                files = Some(total);
            }
            b"symlink path" => return Err(TorrentError::Invalid),
            b"attr" => {
                if value.try_into_bytes()?.contains(&b'l') {
                    return Err(TorrentError::Invalid);
                }
            }
            _ => drop(value),
        }
    }
    drop(info);
    if decoder.next_object()?.is_some() {
        return Err(TorrentError::Invalid);
    }
    let total = match (length, files) {
        (Some(n), None) | (None, Some(n)) => n,
        _ => return Err(TorrentError::Invalid),
    };
    let piece_length = piece_length
        .filter(|n| *n > 0)
        .ok_or(TorrentError::Invalid)?;
    let count = total.div_ceil(piece_length);
    if !name || count.checked_mul(20) != pieces {
        return Err(TorrentError::Invalid);
    }
    Ok(())
}
fn number(value: Object<'_, '_>) -> Result<u64, TorrentError> {
    value
        .try_into_integer()?
        .parse()
        .map_err(|_| TorrentError::Invalid)
}
fn component(value: &[u8]) -> Result<(), TorrentError> {
    if value.is_empty()
        || value.len() > 255
        || matches!(value, b"." | b"..")
        || value
            .iter()
            .any(|b| b.is_ascii_control() || b"/\\:".contains(b))
    {
        Err(TorrentError::Invalid)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashes_exact_original_info_and_rejects_noncanonical_or_v2() {
        let info =
            b"d6:lengthi1e4:name5:a.cbz12:piece lengthi16384e6:pieces20:01234567890123456789e";
        let mut bytes = b"d4:info".to_vec();
        bytes.extend_from_slice(info);
        bytes.push(b'e');
        assert_eq!(
            v1_infohash(&bytes).unwrap(),
            Sha1::digest(info)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert!(v1_infohash(b"d4:infod4:name1:a6:lengthi1eee").is_err());
        assert!(v1_infohash(b"d4:infod6:lengthi01e4:name1:aee").is_err());
        assert!(v1_infohash(b"d4:infode4:infodee").is_err());
        assert_eq!(
            v1_infohash(b"d4:infod12:meta versioni2eee"),
            Err(TorrentError::UnsupportedV2)
        );
        bytes.extend_from_slice(b"de");
        assert!(v1_infohash(&bytes).is_err());
    }
}
