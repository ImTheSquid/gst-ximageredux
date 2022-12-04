use gst::{glib, prelude::StaticType};

mod imp;

glib::wrapper! {
    pub struct XImageRedux(ObjectSubclass<imp::XImageRedux>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl Default for XImageRedux {
    fn default() -> Self {
        glib::Object::new::<XImageRedux>(&[])
    }
}

unsafe impl Send for XImageRedux {}
unsafe impl Sync for XImageRedux {}


pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ximageredux",
        gst::Rank::None,
        XImageRedux::static_type(),
    )
}