use simple_graphics_protocol::{
    ClientRequest, InputResource, Resource, ServerMessage, serialize, serialize_framed,
};

fn main() {
    let cases: Vec<(String, Vec<u8>)> = vec![
        (
            "Acquire{Fbdev}".into(),
            serialize(&ClientRequest::Acquire {
                resource: Resource::Fbdev,
            })
            .unwrap(),
        ),
        (
            "Acquire{Drm0}".into(),
            serialize(&ClientRequest::Acquire {
                resource: Resource::Drm { card: 0 },
            })
            .unwrap(),
        ),
        (
            "Acquire{InputMouse1}".into(),
            serialize(&ClientRequest::Acquire {
                resource: Resource::Input(InputResource::Mouse(1)),
            })
            .unwrap(),
        ),
        (
            "Release{Drm0}".into(),
            serialize(&ClientRequest::Release {
                resource: Resource::Drm { card: 0 },
            })
            .unwrap(),
        ),
        ("Ack".into(), serialize(&ClientRequest::Ack).unwrap()),
        (
            "Advertise{[Drm0,Fbdev]}".into(),
            serialize(&ServerMessage::Advertise {
                available_resources: vec![
                    Resource::Drm { card: 0 },
                    Resource::Fbdev,
                    Resource::Input(InputResource::Mouse(0)),
                ],
            })
            .unwrap(),
        ),
        (
            "Grant{Drm0}".into(),
            serialize(&ServerMessage::Grant {
                resource: Resource::Drm { card: 0 },
            })
            .unwrap(),
        ),
        (
            "Revoke{Drm0}".into(),
            serialize(&ServerMessage::Revoke {
                resource: Resource::Drm { card: 0 },
            })
            .unwrap(),
        ),
        (
            "Deny{owned}".into(),
            serialize(&ServerMessage::Deny {
                reason: "owned".into(),
            })
            .unwrap(),
        ),
    ];

    for (name, payload) in &cases {
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(payload);
        println!(
            "{name:22} payload len={:3}  {}",
            payload.len(),
            hex(payload)
        );
        println!("{name:22} framed  len={:3}  {}", framed.len(), hex(&framed));
    }

    // Sanity: serialize_framed produces the same bytes as manual framing.
    let framed = serialize_framed(&ServerMessage::Grant {
        resource: Resource::Drm { card: 0 },
    })
    .unwrap();
    let mut manual = Vec::with_capacity(4 + cases[6].1.len());
    manual.extend_from_slice(&(cases[6].1.len() as u32).to_be_bytes());
    manual.extend_from_slice(&cases[6].1);
    assert_eq!(framed, manual);
    println!("serialize_framed matches manual framing: yes");
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
