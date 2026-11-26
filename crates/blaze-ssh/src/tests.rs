use blaze_own::{AString, Vec};
use blaze_sim::{Config, check};

use crate::{
    ConfigLimits, ConfigLoader, FrameCodec, FrameLimits, LoadStep, Message, RemoteCompletion,
    RemoteGitSummary, glob_match, parse_config, parse_known_hosts,
};

#[test]
fn parse_openssh_config_first_match_wins_wildcards_and_includes() {
    let alloc = blaze_own::testing::heap().alloc();
    let root = r#"
# Main SSH config
Include conf.d/*.conf

Host prod-api prod-db
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_ed25519_prod

Host prod-*
    HostName %h.internal.example.com
    ProxyJump bastion
    User fallback_user

Host * !bastion
    ForwardAgent yes
    Port 22
"#;
    let inc = r#"
Host bastion
    HostName 10.0.0.1
    User ops
    Port 2200
"#;

    let mut loader = ConfigLoader::new(root, ConfigLimits::SHIPPED, alloc);
    let cfg = loop {
        match loader.poll() {
            LoadStep::NeedInclude(pat) => {
                assert_eq!(pat.as_str(), "conf.d/*.conf");
                loader.feed_files(&[inc], 2);
            }
            LoadStep::Done(c) => break c,
        }
    };

    let prod = cfg.resolve("prod-api", alloc);
    assert_eq!(prod.user.as_ref().map(|s| s.as_str()), Some("deploy"));
    assert_eq!(prod.port, Some(2222));
    assert_eq!(
        prod.hostname.as_ref().map(|s| s.as_str()),
        Some("prod-api.internal.example.com")
    );
    assert_eq!(prod.proxy_jump.as_ref().map(|s| s.as_str()), Some("bastion"));
    assert_eq!(prod.forward_agent, Some(true));

    let bastion = cfg.resolve("bastion", alloc);
    assert_eq!(bastion.hostname.as_ref().map(|s| s.as_str()), Some("10.0.0.1"));
    assert_eq!(bastion.port, Some(2200));
    assert_eq!(bastion.forward_agent, None); // negated by `!bastion`

    let hosts = cfg.list_hosts(32, alloc);
    let names: std::vec::Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(names, ["prod-api", "prod-db", "bastion"]);
    assert!(glob_match("prod-*", "prod-api"));
    assert!(!glob_match("prod-*", "staging-api"));
}

#[test]
fn parse_known_hosts_skips_hashed_and_extracts_ports() {
    let alloc = blaze_own::testing::heap().alloc();
    let raw = "\
# comment
|1|hashed== ssh-ed25519 AAAA
github.com,140.82.121.3 ssh-ed25519 AAAA
[blaze-vm.local]:2222 ssh-ed25519 AAAA
@revoked old.example.com ssh-rsa AAAA
";
    let hosts = parse_known_hosts(raw, 32, alloc);
    assert_eq!(hosts.len(), 3);
    assert_eq!(hosts[0].host.as_str(), "github.com");
    assert_eq!(hosts[0].port, None);
    assert_eq!(hosts[2].host.as_str(), "blaze-vm.local");
    assert_eq!(hosts[2].port, Some(2222));
}

#[test]
fn bsh1_protocol_round_trips_chunked_stream() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut wire = Vec::new_in(alloc);

    let m1 = Message::Hello { version: 1, client_id: AString::from_str_in("blaze-shell", alloc) };
    let mut items = Vec::new_in(alloc);
    items.push(RemoteCompletion {
        label: AString::from_str_in("crates/", alloc),
        detail: AString::from_str_in("dir", alloc),
        is_dir: true,
    });
    let m2 = Message::CompleteResp { req_id: 7, items };
    let m3 = Message::PromptResp {
        req_id: 9,
        summary: RemoteGitSummary {
            cwd: AString::from_str_in("/srv/app", alloc),
            branch: AString::from_str_in("main", alloc),
            dirty_count: 3,
            stashed: 1,
        },
    };

    FrameCodec::encode(&m1, &mut wire, alloc);
    FrameCodec::encode(&m2, &mut wire, alloc);
    FrameCodec::encode(&m3, &mut wire, alloc);

    let mut codec = FrameCodec::new(FrameLimits::SHIPPED, alloc);
    let mut decoded = std::vec::Vec::new();
    // Feed 3 bytes at a time to test streaming reassembly.
    for chunk in wire.chunks(3) {
        codec.push_bytes(chunk);
        while let Some(msg) = codec.next_message().expect("valid frame") {
            decoded.push(msg);
        }
    }
    assert_eq!(decoded.len(), 3);
    assert_eq!(decoded[0], m1);
    assert_eq!(decoded[1], m2);
    assert_eq!(decoded[2], m3);
}

#[test]
fn sim_hostile_ssh_config_and_frames_never_panic() {
    check(Config::cases(120), |tape| {
        let alloc = blaze_own::testing::heap().alloc();
        let len = tape.int_in(0..=160, "len") as usize;
        let mut s = std::string::String::with_capacity(len);
        let alphabet = b"HostName User Port Include Match * ! ? = \" # \n\t a1.-/%";
        for _ in 0..len {
            let idx = tape.below(alphabet.len() as u64, "ch") as usize;
            s.push(alphabet[idx] as char);
        }
        let cfg = parse_config(&s, ConfigLimits::SHIPPED, alloc);
        let _ = cfg.list_hosts(16, alloc);
        let _ = parse_known_hosts(&s, 16, alloc);

        let mut codec = FrameCodec::new(FrameLimits { max_payload_bytes: 256 }, alloc);
        codec.push_bytes(s.as_bytes());
        let _ = codec.next_message();
        Ok::<(), std::string::String>(())
    });
}
