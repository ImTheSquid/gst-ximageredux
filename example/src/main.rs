use std::env;

use gst::{prelude::{Cast, ObjectExt, GstBinExtManual}, Element, traits::ElementExt};

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("Invalid usage!");
        println!("Usage: {} xid_to_capture", args[0]);
        return;
    }

    let xid = if args[1].starts_with("0x") {
        u32::from_str_radix(args[1].trim_start_matches("0x"), 16).expect("Failed to parse hex string!")
    } else {
        args[1].parse().expect("Failed to parse u32!")
    };

    gst::init().unwrap();

    let pipeline = gst::Pipeline::new(None);

    let ximageredux = ximageredux::XImageRedux::default();
    ximageredux.set_property("xid", xid);

    let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let queue = gst::ElementFactory::make("queue").build().unwrap();
    let ximagesink = gst::ElementFactory::make("ximagesink").build().unwrap();

    pipeline.add_many(&[
        ximageredux.upcast_ref::<gst::Element>(), 
        &videoconvert, 
        &queue,
        &ximagesink
    ]).unwrap();

    Element::link_many(&[ximageredux.upcast_ref::<gst::Element>(), &videoconvert, &queue, &ximagesink]).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    ximageredux.connect("resize", false, |value| {
        let width = value[1].get::<u32>().unwrap();
        let height = value[2].get::<u32>().unwrap();
        println!("Resize detected! New size: {}x{}", width, height);

        None
    });

    ximageredux.connect_notify(None, |x, param| {
        match param.name() {
            "width" => println!("New width: {}", x.property::<u32>("width")),
            "height" => println!("New height: {}", x.property::<u32>("height")),
            "visibility" => println!("New visibility: {:?}", x.property::<ximageredux::WindowVisibility>("visibility")),
            _ => unreachable!()
        }
    });

    tokio::signal::ctrl_c().await.unwrap();
}
