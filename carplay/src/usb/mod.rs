//! USB gadget layer for the CarPlay transmitter.
//!
//! Presents the OrangePi 5 Plus USB-C OTG port to the car head unit as an
//! **Apple device (iPhone)** so the car initiates its CarPlay attach sequence.
//!
//! - [`gadget`] — configfs gadget (Apple VID/PID/strings, bind/unbind to UDC).
//! - [`ffs`] — FunctionFS: minimal descriptors + the ep0 control-request loop
//!   that captures and logs everything the car sends, including the CarPlay
//!   USB role-switch vendor request.

pub mod gadget;
pub mod ffs;
pub mod hid_iap2;
pub mod aoa_probe;
