use super::super::*;

fn entry(id: &str, body: &[u8]) -> Vec<u8> {
    let mut inner = vec![0x0a, id.len() as u8];
    inner.extend_from_slice(id.as_bytes());
    inner.extend_from_slice(&[0x12, body.len() as u8]);
    inner.extend_from_slice(body);
    let mut out = vec![0x0a];
    // inner may exceed 127: two-byte varint.
    let n = inner.len();
    if n < 128 {
        out.push(n as u8);
    } else {
        out.extend_from_slice(&[(n & 0x7f) as u8 | 0x80, (n >> 7) as u8]);
    }
    out.extend(inner);
    out
}

#[test]
fn removes_exactly_one_entry_and_nothing_else() {
    let ids: Vec<String> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(super::super::super::state_dir(home.path())).unwrap();
    let file = super::super::super::state_dir(home.path()).join("jetbox_summaries_proto.pb");
    let parts = [
        entry(&ids[0], &[1; 5]),
        entry(&ids[1], &[7; 200]),
        entry(&ids[2], &[3; 9]),
    ];
    std::fs::write(&file, parts.concat()).unwrap();
    assert!(remove_cached_summary(home.path(), &ids[1]).unwrap());
    assert_eq!(
        std::fs::read(&file).unwrap(),
        [parts[0].clone(), parts[2].clone()].concat()
    );
    // Absent id: unchanged.
    assert!(!remove_cached_summary(home.path(), &ids[1]).unwrap());
    assert!(!remove_cached_summary(home.path(), &uuid::Uuid::new_v4().to_string()).unwrap());
}

#[test]
fn an_unexpected_shape_is_left_alone() {
    let id = uuid::Uuid::new_v4().to_string();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(super::super::super::state_dir(home.path())).unwrap();
    let file = super::super::super::state_dir(home.path()).join("jetbox_summaries_proto.pb");
    let mut bytes = entry(&id, &[1; 5]);
    bytes.extend_from_slice(&[0x12, 0x01, 0x00]); // a different top-level field
    std::fs::write(&file, &bytes).unwrap();
    assert!(!remove_cached_summary(home.path(), &id).unwrap());
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    let mut truncated = entry(&id, &[1; 5]);
    truncated.truncate(truncated.len() - 2);
    std::fs::write(&file, &truncated).unwrap();
    assert!(!remove_cached_summary(home.path(), &id).unwrap());
    assert_eq!(std::fs::read(&file).unwrap(), truncated);
}
