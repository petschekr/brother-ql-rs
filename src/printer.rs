use std::fmt;
use std::time::Duration;

pub mod constants;

pub enum Error {
	USB(libusb::Error),
	Message(&'static str),
}
impl fmt::Debug for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Error::USB(ref usb_error) => write!(f, "{:?}", *usb_error),
			Error::Message(s) => write!(f, "{}", s),
		}
	}
}
impl From<libusb::Error> for Error {
	fn from(err: libusb::Error) -> Error {
		Error::USB(err)
	}
}
impl From<&'static str> for Error {
	fn from(err: &'static str) -> Error {
		Error::Message(err)
	}
}

#[allow(non_snake_case)]
mod Status {
	use super::constants::*;
	#[derive(Debug)]
	pub enum MediaType {
		None,
		ContinuousTape,
		DieCutLabels,
	}

	#[derive(Debug)]
	pub struct Media {
		pub media_type: MediaType,
		pub width: u8,
		pub length: u8,
	}
	impl Media {
		pub fn to_label(&self) -> Label {
			let length = if self.length == 0 {
				None
			}
			else {
				Some(self.length)
			};
			label_data(self.width, length).expect("Printer reported invalid label dimensions")
		}
	}

	#[derive(Debug)]
	pub enum StatusType {
		ReplyToStatusRequest,
		PrintingCompleted,
		ErrorOccurred,
		Notification,
		PhaseChange,
	}

	#[derive(Debug)]
	pub struct Response {
		pub model: &'static str,
		pub status_type: StatusType,
		pub errors: Vec<&'static str>,
		pub media: Media,
	}
}

pub struct PrinterManager {
	context: libusb::Context,
}

impl PrinterManager {
	pub fn new() -> Result<Self, Error> {
		let context = libusb::Context::new()?;
		Ok(Self {
			context,
		})
	}

	fn printer_filter(device: &libusb::Device) -> bool {
		let descriptor = device.device_descriptor().unwrap();
		if descriptor.vendor_id() == constants::VENDOR_ID && descriptor.product_id() == 0x2049 {
			eprintln!("You must disable Editor Lite mode on your QL-700 before you can print with it");
		}
		descriptor.vendor_id() == constants::VENDOR_ID && constants::printer_name_from_id(descriptor.product_id()).is_some()
	}

	pub fn available_devices(&self) -> Result<u8, Error> {
		let devices = self.context.devices()?;
		let devices = devices.iter().filter(PrinterManager::printer_filter);
		Ok(devices.count() as u8)
	}

	pub fn get<F>(&self, index: u8, callback: F) -> ()
		where F: FnOnce(ThermalPrinter) -> ()
	{
		let device = self.context
			.devices().expect("Failed to get devices")
			.iter()
			.filter(PrinterManager::printer_filter)
			.nth(index as usize).expect("No printer found at index");
		let mut printer = ThermalPrinter::new(device).unwrap();
		printer.init().unwrap();
		callback(printer);
	}
}

const RASTER_LINE_LENGTH: u8 = 90;

pub struct ThermalPrinter<'d> {
	device: libusb::Device<'d>,
	handle: libusb::DeviceHandle<'d>,
	in_endpoint: Option<u8>,
	out_endpoint: Option<u8>,
}
impl<'d> ThermalPrinter<'d> {
	pub fn new(device: libusb::Device<'d>) -> Result<Self, Error> {
		Ok(ThermalPrinter {
			handle: device.open()?,
			device,
			in_endpoint: None,
			out_endpoint: None,
		})
	}

	pub fn init(&mut self) -> Result<Status::Response, Error> {
		let config = self.device.active_config_descriptor()?;
		let interface = config.interfaces().next().expect("Brother QL printers should have exactly one interface");
		let interface_descriptor = interface.descriptors().next().expect("Brother QL printers should have exactly one interface descriptor");
		for endpoint in interface_descriptor.endpoint_descriptors() {
			assert_eq!(endpoint.transfer_type(), libusb::TransferType::Bulk, "Brother QL printers are defined as using bulk endpoint communication");
			match endpoint.direction() {
				libusb::Direction::In  => self.in_endpoint  = Some(endpoint.address()),
				libusb::Direction::Out => self.out_endpoint = Some(endpoint.address()),
			}
		}
		assert!(self.in_endpoint.is_some() && self.out_endpoint.is_some(), "Input/output endpoints not found");

		self.handle.claim_interface(interface.number())?;
		if let Ok(kd_active) = self.handle.kernel_driver_active(interface.number()) {
			if kd_active {
				self.handle.detach_kernel_driver(interface.number())?;
			}
		}

		// Reset printer
		let clear_command = [0x00; 200];
		self.write(&clear_command)?;
		let initialize_command = [0x1B, 0x40];
		self.write(&initialize_command)?;

		self.get_status()
	}

	pub fn print(&self, raster_lines: Vec<[u8; RASTER_LINE_LENGTH as usize]>) -> Result<Status::Response, Error> {
		let status = self.get_status()?;

		let mode_command = [0x1B, 0x69, 0x61, 1];
		self.write(&mode_command)?;

		const VALID_FLAGS: u8 = 0x80 | 0x02 | 0x04 | 0x08 | 0x40; // Everything enabled
		let media_type: u8 = match status.media.media_type {
			Status::MediaType::ContinuousTape => 0x0A,
			Status::MediaType::DieCutLabels => 0x0B,
			_ => return Err("No media loaded into printer".into())
		};

		let mut media_command = [0x1B, 0x69, 0x7A, VALID_FLAGS, media_type, status.media.width, status.media.length, 0, 0, 0, 0, 0x01, 0];
		let line_count = (raster_lines.len() as u32).to_le_bytes();
		media_command[7..7 + 4].copy_from_slice(&line_count);
		self.write(&media_command)?;

		self.write(&[0x1B, 0x69, 0x4D, 1 << 6])?; // Enable auto-cut
		self.write(&[0x1B, 0x69, 0x4B, 1 << 3 | 0 << 6])?; // Enable cut-at-end and disable high res printing

		let label = self.current_label()?;

		let margins_command = [0x1B, 0x69, 0x64, label.feed_margin, 0];
		self.write(&margins_command)?;

		for line in raster_lines.iter() {
			let mut raster_command = vec![0x67, 0x00, RASTER_LINE_LENGTH];
			raster_command.extend_from_slice(line);
			self.write(&raster_command)?;
		}

		let print_command = [0x1A];
		self.write(&print_command)?;

		self.read()
	}

	pub fn current_label(&self) -> Result<constants::Label, Error> {
		let media = self.get_status()?.media;
		constants::label_data(media.width, match media.length {
			0 => None,
			_ => Some(media.length)
		}).ok_or("Unknown media loaded in printer".into())
	}

	pub fn get_status(&self) -> Result<Status::Response, Error> {
		let status_command = [0x1B, 0x69, 0x53];
		self.write(&status_command)?;
		self.read()
	}

	fn read(&self) -> Result<Status::Response, Error> {
		const RECEIVE_SIZE: usize = 32;
		let mut response = [0; RECEIVE_SIZE];
		let bytes_read = self.handle.read_bulk(self.in_endpoint.unwrap(), &mut response, Duration::from_millis(500))?;

		if bytes_read != RECEIVE_SIZE || response[0] != 0x80 {
			return Err("Invalid response received from printer".into());
		}

		let model = match response[4] {
			0x4F => "QL-500/550",
			0x31 => "QL-560",
			0x32 => "QL-570",
			0x33 => "QL-580N",
			0x51 => "QL-650TD",
			0x35 => "QL-700",
			0x50 => "QL-1050",
			0x34 => "QL-1060N",
			_ => "Unknown"
		};

		let mut errors = Vec::new();

		fn error_if(byte: u8, flag: u8, message: &'static str, errors: &mut Vec<&'static str>) {
			if byte & flag != 0 {
				errors.push(message);
			}
		}
		error_if(response[8], 0x01, "No media when printing", &mut errors);
		error_if(response[8], 0x02, "End of media", &mut errors);
		error_if(response[8], 0x04, "Tape cutter jam", &mut errors);
		error_if(response[8], 0x10, "Main unit in use", &mut errors);
		error_if(response[8], 0x80, "Fan doesn't work", &mut errors);
		error_if(response[9], 0x04, "Transmission error", &mut errors);
		error_if(response[9], 0x10, "Cover open", &mut errors);
		error_if(response[9], 0x40, "Cannot feed", &mut errors);
		error_if(response[9], 0x80, "System error", &mut errors);

		let width = response[10];
		let length = response[17];

		let media_type = match response[11] {
			0x0A => Status::MediaType::ContinuousTape,
			0x0B => Status::MediaType::DieCutLabels,
			_    => Status::MediaType::None,
		};

		let status_type = match response[18] {
			0x00 => Status::StatusType::ReplyToStatusRequest,
			0x01 => Status::StatusType::PrintingCompleted,
			0x02 => Status::StatusType::ErrorOccurred,
			0x05 => Status::StatusType::Notification,
			0x06 => Status::StatusType::PhaseChange,
			// Will never occur
			_ => Status::StatusType::Notification
		};

		Ok(Status::Response {
			model,
			status_type,
			errors,
			media: Status::Media {
				media_type,
				width,
				length,
			}
		})
	}

	fn write(&self, data: &[u8]) -> Result<(), Error> {
		self.handle.write_bulk(self.out_endpoint.unwrap(), data, Duration::from_millis(500))?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use crate::printer::ThermalPrinter;
	#[test]
	fn connect() {
		let context = libusb::Context::new().unwrap();
		let mut printer = ThermalPrinter::new(&context).unwrap();
		let available = printer.available_devices().unwrap();
		assert!(dbg!(available) > 0, "No printers found");
		printer.init(0).unwrap();
	}

	use std::path::PathBuf;
    #[test]
	#[ignore]
    fn print() {
		let context = libusb::Context::new().unwrap();
		let mut printer = ThermalPrinter::new(&context).unwrap();
		let available = printer.available_devices().unwrap();
		assert!(dbg!(available) > 0, "No printers found");
		let label = printer.init(0).unwrap().media.to_label();

        let mut rasterizer = crate::text::TextRasterizer::new(
            label,
            PathBuf::from("./Space Mono Bold.ttf")
        );
        rasterizer.set_second_row_image(PathBuf::from("./logos/BuildGT Mono.png"));
        let lines = rasterizer.rasterize(
            "Ryan Petschek",
            Some("Computer Science"),
            1.2
        );

		dbg!(printer.print(lines).unwrap());
    }
}
