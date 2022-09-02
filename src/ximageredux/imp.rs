use std::sync::Mutex;

use derivative::Derivative;
use gst::{glib, subclass::prelude::{ObjectSubclass, ElementImpl, ObjectImpl, GstObjectImpl, ObjectImplExt}, prelude::ToValue};
use gst_app::prelude::{BaseSinkExt, BaseSrcExt};
use gst_base::{BaseSrc, subclass::prelude::BaseSrcImpl};
use once_cell::sync::Lazy;


type Xid = u32;

#[derive(Derivative)]
#[derivative(Default)]
struct State {
    connection: Option<xcb::Connection>,
    xid: Option<Xid>,
    #[derivative(Default(value="true"))]
    needs_bounds_update: bool,
    width: u32,
    height: u32
}

#[derive(Default)]
pub struct XImageRedux {
    state: Mutex<State>
}


#[glib::object_subclass]
impl ObjectSubclass for XImageRedux {
    const NAME: &'static str = "XImageRedux";
    type Type = super::XImageRedux;
    type ParentType = BaseSrc;
}

impl BaseSrcImpl for XImageRedux {
    fn is_seekable(&self, _element: &Self::Type) -> bool {
        false
    }

    fn start(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        let connection = match xcb::Connection::connect(None) {
            Ok((c, _)) => c,
            Err(e) => return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [&format!("Failed to connect to X11 server: {}", e.to_string())]
            ))
        };

        let _ = state.connection.insert(connection);

        Ok(())
    }

    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.state.lock().unwrap().connection.take();

        Ok(())
    }

    fn create(
            &self,
            element: &Self::Type,
            offset: u64,
            buffer: Option<&mut gst::BufferRef>,
            length: u32,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
        todo!()
    }

    fn fill(
            &self,
            element: &Self::Type,
            offset: u64,
            length: u32,
            buffer: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
        todo!()
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

impl ObjectImpl for XImageRedux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecString::builder("xid")
                .nick("XID")
                .blurb("XID of window to capture")
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _obj: &Self::Type, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "xid" => match value.get::<Xid>() {
                Ok(xid) => {
                    let mut state = self.state.lock().unwrap();
                    let _ = state.xid.insert(xid);
                    state.needs_bounds_update = true;
                }
                Err(e) => panic!("Attempted to set xid with type {}, requires {}", e.actual_type(), e.requested_type()),
            }
            _ => unimplemented!()
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "xid" => self.state.lock().unwrap().xid.unwrap_or(0).to_value(),
            _ => unimplemented!()
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        obj.set_live(true);
        obj.set_format(gst::Format::Bytes);
    }
}

impl GstObjectImpl for XImageRedux {}