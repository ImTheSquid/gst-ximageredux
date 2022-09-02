use gst::{glib, subclass::prelude::{ObjectSubclass, ElementImpl, ObjectImpl, GstObjectImpl}};
use gst_base::{BaseSrc, subclass::prelude::BaseSrcImpl};
use once_cell::sync::Lazy;

#[derive(Default)]
pub struct XImageRedux;


#[glib::object_subclass]
impl ObjectSubclass for XImageRedux {
    const NAME: &'static str = "XImageRedux";
    type Type = super::XImageRedux;
    type ParentType = BaseSrc;
}

impl BaseSrcImpl for XImageRedux {
    fn is_seekable(&self, element: &Self::Type) -> bool {
        false
    }

    fn start(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        Ok(())
    }
}

impl ElementImpl for XImageRedux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "X11 Window Capture Engine",
                "Source/Video",
                "Captures X11 windows",
                "Jack Hogan",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            vec![]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl ObjectImpl for XImageRedux {}

impl GstObjectImpl for XImageRedux {}