//! Unit tests for the mDNS responder / browser logic (pure parts plus a
//! loopback round trip that never touches the real network).

use super::*;

const ID: u64 = 987_654_321_012;

fn cfg() -> AdvertiseConfig {
    let mut c = AdvertiseConfig::new(ID, 4570);
    c.name = "Office PC".into();
    c.addrs = vec![
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
    ];
    c
}

fn records() -> ServiceRecords {
    let c = cfg();
    ServiceRecords::new(&c, &c.publish_addrs(&[]))
}

fn query(name: &str, qtype: u16) -> Message {
    Message {
        id: 0x1234,
        questions: vec![Question {
            name: name.into(),
            qtype,
            unicast_response: false,
        }],
        ..Message::default()
    }
}

fn src() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))
}

#[test]
fn names_and_txt() {
    assert_eq!(instance_name(42), "42._openview._udp.local");
    assert_eq!(host_name(42), "openview-42.local");
    let entries = cfg().txt_entries();
    assert_eq!(entries[0], format!("id={ID}").into_bytes());
    assert_eq!(entries[1], b"v=2".to_vec());
    assert_eq!(entries[2], b"name=Office PC".to_vec());
}

#[test]
fn long_display_name_fits_one_txt_string() {
    let mut c = cfg();
    c.name = "é".repeat(300);
    for e in c.txt_entries() {
        assert!(e.len() <= 255);
        assert!(std::str::from_utf8(&e).is_ok(), "cut on a char boundary");
    }
}

#[test]
fn truncate_respects_char_boundaries() {
    assert_eq!(truncate_utf8("abc", 10), "abc");
    assert_eq!(truncate_utf8("abc", 2), "ab");
    assert_eq!(truncate_utf8("aé", 2), "a");
}

#[test]
fn publish_addrs_dedupes_and_enumerates() {
    let mut c = cfg();
    c.addrs.push(c.addrs[0]);
    assert_eq!(c.publish_addrs(&[]).len(), 2);
    let ifaces = vec![LocalInterface {
        name: "Ethernet".into(),
        index: 3,
        v4: vec![(Ipv4Addr::new(192, 168, 7, 9), 24)],
        v6: vec!["2001:db8::9".parse().unwrap()],
    }];
    let mut e = AdvertiseConfig::new(1, 4570);
    assert_eq!(
        e.publish_addrs(&ifaces),
        vec![IpAddr::V4(Ipv4Addr::new(192, 168, 7, 9))]
    );
    e.include_ipv6 = true;
    assert_eq!(e.publish_addrs(&ifaces).len(), 2);
}

#[test]
fn ptr_query_gets_ptr_plus_srv_txt_a() {
    let r = records();
    let reply = r.respond(&query(SERVICE_TYPE, TYPE_PTR), false).unwrap();
    assert!(reply.is_response());
    assert_eq!(reply.id, 0);
    assert!(reply.questions.is_empty());
    assert_eq!(reply.answers.len(), 1);
    assert_eq!(reply.answers[0].data, RData::Ptr(instance_name(ID)));
    assert!(!reply.answers[0].cache_flush, "PTR is shared");
    let types: Vec<u16> = reply.additionals.iter().map(|a| a.rtype).collect();
    assert_eq!(types, vec![TYPE_SRV, TYPE_TXT, TYPE_A, TYPE_A]);
    assert!(reply.additionals.iter().all(|a| a.cache_flush));
}

#[test]
fn service_name_matching_is_case_insensitive() {
    let r = records();
    assert!(r
        .respond(&query("_OpenView._UDP.local", TYPE_PTR), false)
        .is_some());
    assert!(r.respond(&query(SERVICE_TYPE, TYPE_ANY), false).is_some());
}

#[test]
fn legacy_unicast_reply_echoes_query_and_caps_ttl() {
    let r = records();
    let q = query(SERVICE_TYPE, TYPE_PTR);
    let reply = r.respond(&q, true).unwrap();
    assert_eq!(reply.id, 0x1234);
    assert_eq!(reply.questions, q.questions);
    for rec in reply.records() {
        assert!(rec.ttl <= TTL_LEGACY_MAX);
        assert!(!rec.cache_flush);
    }
}

#[test]
fn instance_host_and_meta_queries() {
    let r = records();
    let srv = r
        .respond(&query(&instance_name(ID), TYPE_SRV), false)
        .unwrap();
    assert_eq!(srv.answers.len(), 1);
    assert_eq!(srv.answers[0].rtype, TYPE_SRV);
    assert!(srv.additionals.iter().any(|a| a.rtype == TYPE_TXT));
    assert!(srv.additionals.iter().any(|a| a.rtype == TYPE_A));

    let any = r
        .respond(&query(&instance_name(ID), TYPE_ANY), false)
        .unwrap();
    assert_eq!(any.answers.len(), 2);

    let a = r.respond(&query(&host_name(ID), TYPE_A), false).unwrap();
    assert_eq!(a.answers.len(), 2);
    assert!(a.additionals.is_empty());
    assert!(r
        .respond(&query(&host_name(ID), TYPE_AAAA), false)
        .is_none());

    let meta = r.respond(&query(META_SERVICES, TYPE_PTR), false).unwrap();
    assert_eq!(meta.answers[0].data, RData::Ptr(SERVICE_TYPE.into()));
}

#[test]
fn unrelated_queries_and_responses_are_ignored() {
    let r = records();
    assert!(r
        .respond(&query("_googlecast._tcp.local", TYPE_PTR), false)
        .is_none());
    assert!(r
        .respond(&query(&instance_name(ID + 1), TYPE_SRV), false)
        .is_none());
    assert!(r.respond(&query(SERVICE_TYPE, TYPE_A), false).is_none());
    let mut resp = query(SERVICE_TYPE, TYPE_PTR);
    resp.flags = FLAG_RESPONSE;
    assert!(r.respond(&resp, false).is_none(), "never answer a response");
}

#[test]
fn known_answer_suppression() {
    let r = records();
    let mut q = query(SERVICE_TYPE, TYPE_PTR);
    q.answers.push(r.ptr.clone());
    assert!(
        r.respond(&q, false).is_none(),
        "fresh known answer suppresses"
    );
    q.answers[0].ttl = TTL_OTHER / 2 - 1;
    assert!(
        r.respond(&q, false).is_some(),
        "stale known answer does not"
    );
}

#[test]
fn announcement_and_goodbye() {
    let r = records();
    let hello = r.announcement(false);
    assert_eq!(hello.answers.len(), 5);
    assert!(hello.answers.iter().all(|a| a.ttl > 0));
    let bye = r.announcement(true);
    assert!(bye.answers.iter().all(|a| a.ttl == 0));
    assert!(bye.encode().is_ok());
}

#[test]
fn response_round_trips_into_a_discovered_host() {
    let r = records();
    let wire = r
        .respond(&query(SERVICE_TYPE, TYPE_PTR), true)
        .unwrap()
        .encode()
        .unwrap();
    let mut cache = Cache::default();
    cache.ingest(&Message::parse(&wire).unwrap(), src(), Instant::now());
    let hosts = cache.hosts();
    assert_eq!(hosts.len(), 1);
    let h = &hosts[0];
    assert_eq!(h.device_id, ID);
    assert_eq!(h.name, "Office PC");
    assert_eq!(h.version, Some(2));
    assert!(h.is_compatible());
    assert_eq!(
        h.addrs,
        vec![
            "192.168.1.20:4570".parse().unwrap(),
            "10.0.0.5:4570".parse().unwrap()
        ]
    );
}

#[test]
fn packet_source_address_is_tried_first() {
    let r = records();
    let msg = r.announcement(false);
    let mut cache = Cache::default();
    cache.ingest(&msg, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), Instant::now());
    let h = &cache.hosts()[0];
    assert_eq!(h.addrs[0], "10.0.0.5:4570".parse().unwrap());
    assert_eq!(h.addrs.len(), 2);
}

#[test]
fn source_address_is_the_fallback_without_a_records() {
    let r = records();
    let mut msg = r.announcement(false);
    msg.answers.retain(|a| a.rtype != TYPE_A);
    let mut cache = Cache::default();
    let from = IpAddr::V4(Ipv4Addr::new(172, 16, 0, 9));
    cache.ingest(&msg, from, Instant::now());
    assert_eq!(cache.hosts()[0].addrs, vec![SocketAddr::new(from, 4570)]);
}

#[test]
fn goodbye_removes_host() {
    let r = records();
    let mut cache = Cache::default();
    let now = Instant::now();
    cache.ingest(&r.announcement(false), src(), now);
    assert_eq!(cache.hosts().len(), 1);
    cache.ingest(&r.announcement(true), src(), now);
    assert!(cache.hosts().is_empty());
}

#[test]
fn entries_expire_after_hold() {
    let r = records();
    let mut cache = Cache::default();
    let now = Instant::now();
    // Legacy-unicast TTLs (10 s) are held for MIN_HOLD.
    let legacy = r.respond(&query(SERVICE_TYPE, TYPE_PTR), true).unwrap();
    cache.ingest(&legacy, src(), now);
    cache.expire(now + Duration::from_secs(20));
    assert_eq!(cache.hosts().len(), 1, "held at least 30 s");
    cache.expire(now + MIN_HOLD + Duration::from_secs(1));
    assert!(cache.hosts().is_empty());
    assert!(cache.hosts.is_empty());
}

#[test]
fn queries_and_foreign_services_are_not_cached() {
    let mut cache = Cache::default();
    let now = Instant::now();
    cache.ingest(&query(SERVICE_TYPE, TYPE_PTR), src(), now);
    let foreign = Message {
        flags: FLAG_RESPONSE,
        answers: vec![
            ResourceRecord::new(
                "printer._ipp._tcp.local",
                120,
                true,
                RData::Srv {
                    priority: 0,
                    weight: 0,
                    port: 631,
                    target: "printer.local".into(),
                },
            ),
            ResourceRecord::new(
                "printer.local",
                120,
                true,
                RData::A(Ipv4Addr::new(1, 2, 3, 4)),
            ),
        ],
        ..Message::default()
    };
    cache.ingest(&foreign, src(), now);
    assert!(cache.instances.is_empty());
    assert!(cache.hosts.is_empty(), "unreferenced A records are dropped");
}

#[test]
fn device_id_falls_back_to_instance_label_and_dedupes() {
    let mut c = cfg();
    c.name = String::new();
    let mut r = ServiceRecords::new(&c, &c.publish_addrs(&[]));
    // TXT without id= / name=.
    r.txt.data = RData::Txt(vec![b"v=2".to_vec(), b"junk".to_vec()]);
    let mut cache = Cache::default();
    cache.ingest(&r.announcement(false), src(), Instant::now());
    let hosts = cache.hosts();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].device_id, ID);
    assert_eq!(hosts[0].name, ID.to_string());

    // A non-numeric label without TXT id is not an OpenView host.
    let mut cache = Cache::default();
    let mut msg = r.announcement(false);
    for a in &mut msg.answers {
        if a.name.eq_ignore_ascii_case(&instance_name(ID)) {
            a.name = format!("abc.{SERVICE_TYPE}");
        }
        if let RData::Ptr(t) = &mut a.data {
            *t = format!("abc.{SERVICE_TYPE}");
        }
    }
    cache.ingest(&msg, src(), Instant::now());
    assert!(cache.hosts().is_empty());
}

#[test]
fn txt_info_parsing() {
    let t = TxtInfo::parse(&[
        b"ID=77".to_vec(),
        b"v=x".to_vec(),
        b"name=  Den  ".to_vec(),
        b"id=78".to_vec(),
    ]);
    assert_eq!(t.id, Some(77), "keys are case-insensitive, first wins");
    assert_eq!(t.version, None);
    assert_eq!(t.name.as_deref(), Some("Den"));
    assert_eq!(
        TxtInfo::parse(&[b"id".to_vec(), b"name=".to_vec()]),
        TxtInfo::default()
    );
}

#[test]
fn instance_key_requires_our_service_suffix() {
    assert_eq!(
        instance_key("42._OpenView._udp.local"),
        Some(("42._openview._udp.local".into(), "42".into()))
    );
    assert_eq!(instance_key("_openview._udp.local"), None);
    assert_eq!(instance_key("42x_openview._udp.local"), None);
    assert_eq!(instance_key("42._openview._tcp.local"), None);
}

#[test]
fn cache_size_is_bounded() {
    let mut cache = Cache::default();
    let now = Instant::now();
    for id in 0..(MAX_CACHE as u64 + 50) {
        let mut c = cfg();
        c.device_id = id;
        let r = ServiceRecords::new(&c, &c.publish_addrs(&[]));
        cache.ingest(&r.announcement(false), src(), now);
    }
    assert_eq!(cache.instances.len(), MAX_CACHE);
}

#[test]
fn browser_drain_skips_malformed_packets() {
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let to = rx.local_addr().unwrap();
    let good = records().announcement(false).encode().unwrap();
    let mut truncated = good.clone();
    truncated.truncate(good.len() / 2);
    tx.send_to(&[0xFF; 3], to).unwrap();
    tx.send_to(&truncated, to).unwrap();
    tx.send_to(&good, to).unwrap();
    let mut cache = Cache::default();
    let mut buf = vec![0u8; MAX_PACKET];
    let deadline = Instant::now() + Duration::from_secs(3);
    while cache.hosts().is_empty() && Instant::now() < deadline {
        Querier::drain(&rx, &mut cache, &mut buf);
    }
    assert_eq!(
        cache.hosts().len(),
        1,
        "valid packet after junk still parsed"
    );
}

#[test]
fn loopback_advertise_and_browse() {
    let mut c = cfg();
    c.device_id = 4_242_424_242;
    let mut adv = Advertiser::start_loopback(c).unwrap();
    assert_eq!(adv.instance_name(), "4242424242._openview._udp.local");
    let responder = adv.local_addr();

    // Junk to the responder must not stop it.
    let junk = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    junk.send_to(&[1, 2, 3], responder).unwrap();
    junk.send_to(&[0u8; 12], responder).unwrap();

    let browser = Browser::start_unicast(&[responder]).unwrap();
    let host = wait_for(&browser, 4_242_424_242, Duration::from_secs(5)).expect("resolved");
    assert_eq!(host.name, "Office PC");
    assert_eq!(host.version, Some(TXT_PROTOCOL_VERSION));
    assert_eq!(host.addrs[0], "192.168.1.20:4570".parse().unwrap());
    assert_eq!(browser.hosts().len(), 1);
    assert!(wait_for(&browser, 1, Duration::from_millis(100)).is_none());
    adv.stop();
}
