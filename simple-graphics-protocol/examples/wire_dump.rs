use simple_graphics_protocol::{
    ClientRequest, Resource, DisplayResource, ServerMessage, serialize, serialize_framed,
};

fn main() {
    let cases: Vec<(String, Vec<u8>)> = vec![
        (
            "Acquire{Fbdev}".into(),
            serialize(&ClientRequest::Acquire {
                resource: Resource::Display(DisplayResource::Fbdev),
            })
            .unwrap(),
        ),
        (
            "Release{Fbdev}".into(),
            serialize(&ClientRequest::Release {
                resource: Resource::Display(DisplayResource::Fbdev),
            })
            .unwrap(),
        ),
        (
            "Ack".into(),
            serialize(&ClientRequest::Ack).unwrap(),
        ),
        (
            "Advertise{[Fbdev]}".into(),
            serialize(&ServerMessage::Advertise {
                available_resources: vec![Resource::Display(DisplayResource::Fbdev)],
            })
            .unwrap(),
        ),
        (
            "Grant{Fbdev}".into(),
            serialize(&ServerMessage::Grant {
                resource: Resource::Display(DisplayResource::Fbdev),
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
        println!("{name:20} payload len={:3}  {}", payload.len(), hex(payload));
        println!("{name:20} framed  len={:3}  {}", framed.len(), hex(&framed));
    }

    // Sanity: serialize_framed produces the same bytes as manual framing.
    let framed = serialize_framed(&ServerMessage::Grant {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .unwrap();
    let mut manual = Vec::with_capacity(4 + cases[3].1.len());
    manual.extend_from_slice(&(cases[3].1.len() as u32).to_be_bytes());
    manual.extend_from_slice(&cases[3].1);
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
