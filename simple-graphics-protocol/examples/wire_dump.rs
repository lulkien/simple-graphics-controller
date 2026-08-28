use simple_graphics_protocol::{ClientRequest, Resource, ServerMessage, serialize};

fn main() {
    let a = serialize(&ClientRequest::Acquire {
        resources: vec![Resource::Fbdev],
    })
    .unwrap();
    let r = serialize(&ClientRequest::Release {
        resources: vec![Resource::Fbdev],
    })
    .unwrap();
    let adv = serialize(&ServerMessage::Advertise {
        available_resources: vec![Resource::Fbdev],
    })
    .unwrap();
    let g = serialize(&ServerMessage::Grant {
        resources: vec![Resource::Fbdev],
    })
    .unwrap();
    let d = serialize(&ServerMessage::Deny {
        reason: "owned".into(),
    })
    .unwrap();
    for (name, b) in [
        ("Acquire{Fbdev}", a),
        ("Release{Fbdev}", r),
        ("Advertise{[Fbdev]}", adv),
        ("Grant{[Fbdev]}", g),
        ("Deny{owned}", d),
    ] {
        println!("{name:20} len={:3}  {}", b.len(), b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" "));
    }
}
