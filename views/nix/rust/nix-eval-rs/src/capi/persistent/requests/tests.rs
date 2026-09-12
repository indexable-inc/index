use super::*;

fn input(requests: serde_json::Value) -> Vec<u8> {
    serde_json::json!({"version": 1, "requests": requests}).to_string().into_bytes()
}

fn request(id: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "installable": ".#value", "apply": null})
}

fn report() -> String {
    serde_json::json!({"installable": ".#value", "value": {"drv": {}, "missing": []}}).to_string()
}

#[test]
fn validates_all_requests_before_exposing_the_first() {
    let mut second = request("second");
    second["unknown"] = true.into();
    let requests = vec![request("first"), second];
    assert!(Batch::parse(&input(requests.into())).is_err());
    for malformed in [
        r#"{"version":1,"version":1,"requests":[]}"#,
        r#"{"version":1,"requests":[{"id":"x","id":"y","installable":".#v","apply":null}]}"#,
        r#"{"version":1,"requests":[{"id":"x","installable":".#v"}]}"#,
        r#"{"version":1,"requests":[{"id":"x","installable":".#v","apply":3}]}"#,
        r#"{"version":2,"requests":[]}"#,
        r#"{"version":1,"requests":[],"extra":null}"#,
    ] {
        assert!(Batch::parse(malformed.as_bytes()).is_err());
    }
    assert!(Batch::parse(&[0xff]).is_err());
}

#[test]
fn enforces_file_count_unique_identity_and_text_limits() {
    assert!(Batch::parse(&vec![b' '; MAX_FILE_BYTES + 1]).is_err());
    assert!(Batch::parse(&input(Vec::<serde_json::Value>::new().into())).is_err());
    assert!(Batch::parse(&input(vec![request("same"), request("same")].into())).is_err());
    for id in [String::new(), "a".repeat(MAX_ID_BYTES + 1), "é".repeat(MAX_ID_BYTES)] {
        assert!(Batch::parse(&input(vec![request(&id)].into())).is_err());
    }
    let requests: Vec<_> = (0..MAX_REQUESTS).map(|n| request(&n.to_string())).collect();
    assert!(Batch::parse(&input(requests.clone().into())).is_ok());
    let mut too_many = requests;
    too_many.push(request("extra"));
    assert!(Batch::parse(&input(too_many.into())).is_err());
    for installable in [String::new(), "x".repeat(MAX_INSTALLABLE_BYTES + 1), "x\0y".to_owned()] {
        let mut row = request("id");
        row["installable"] = installable.into();
        assert!(Batch::parse(&input(vec![row].into())).is_err());
    }
    let mut row = request("id");
    row["apply"] = "x".repeat(MAX_APPLY_BYTES + 1).into();
    assert!(Batch::parse(&input(vec![row].into())).is_err());
    let mut row = request("id");
    row["apply"] = "x\0y".into();
    assert!(Batch::parse(&input(vec![row].into())).is_err());
}

#[test]
fn preserves_exact_apply_and_opaque_ids_and_keeps_one_json_line() -> Result<(), String> {
    let id = "tenant\n\"opaque\0id";
    let apply = "jobs:\n { literal = \"$HOME `tick` $(touch never) '\"; }";
    let mut row = request(id);
    row["apply"] = apply.into();
    let mut batch = Batch::parse(&input(vec![row].into()))?;
    let observed = batch.begin()?.ok_or("request missing")?;
    assert_eq!(observed.id, id);
    assert_eq!(observed.apply.as_deref(), Some(apply));
    let framed = batch.complete(true, &report())?;
    assert_eq!(framed.lines().count(), 1);
    let parsed: serde_json::Value = serde_json::from_str(&framed).map_err(|e| e.to_string())?;
    assert_eq!(parsed["id"], id);
    assert_eq!(parsed["status"], "ok");
    assert!(batch.begin()?.is_none());
    Ok(())
}

#[test]
fn requires_ordered_completion_and_stops_at_first_failure() -> Result<(), String> {
    let mut batch = Batch::parse(&input(vec![request("first"), request("failed"), request("never")].into()))?;
    assert!(batch.complete(true, &report()).is_err());
    assert_eq!(batch.begin()?.ok_or("missing first")?.id, "first");
    assert!(batch.begin().is_err());
    let first = batch.complete(true, &report())?;
    assert_eq!(batch.output_bytes, first.len());
    assert_eq!(batch.begin()?.ok_or("missing second")?.id, "failed");
    let failed = batch.complete(false, "evaluation failed\nnext line")?;
    assert_eq!(batch.output_bytes, first.len() + failed.len());
    let parsed: serde_json::Value = serde_json::from_str(&failed).map_err(|e| e.to_string())?;
    assert_eq!(parsed["id"], "failed");
    assert_eq!(parsed["status"], "error");
    assert!(parsed.get("value").is_none());
    assert!(batch.begin().is_err());
    assert!(batch.complete(true, &report()).is_err());
    Ok(())
}

#[test]
fn wrong_response_identity_and_output_overflow_poison_the_batch() -> Result<(), String> {
    for payload in [
        "[]".to_owned(),
        "{\"installable\":\".#other\",\"value\":0}".to_owned(),
        "{\"installable\":\".#value\",\"value\":0,\"id\":\"forged\"}".to_owned(),
        "x".repeat(MAX_REPORT_BYTES + 1),
    ] {
        let mut batch = Batch::parse(&input(vec![request("one")].into()))?;
        assert!(batch.begin()?.is_some());
        assert!(batch.complete(true, &payload).is_err());
        assert!(batch.begin().is_err());
    }
    let mut batch = Batch::parse(&input(vec![request("one")].into()))?;
    batch.output_bytes = MAX_OUTPUT_BYTES - 1;
    assert!(batch.begin()?.is_some());
    assert!(batch.complete(true, &report()).is_err());
    assert!(batch.begin().is_err());
    Ok(())
}
