use std::fmt;
use std::time::Duration;

mod constants;

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

pub struct ThermalPrinter<'d> {
	context: &'d libusb::Context,
	handle: Option<libusb::DeviceHandle<'d>>,
	in_endpoint: Option<u8>,
	out_endpoint: Option<u8>,
}
impl<'d> ThermalPrinter<'d> {
	pub fn new(context: &'d libusb::Context) -> Result<Self, Error> {
		Ok(ThermalPrinter {
			context,
			handle: None,
			in_endpoint: None,
			out_endpoint: None,
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
		let devices = devices.iter().filter(ThermalPrinter::printer_filter);
		Ok(devices.count() as u8)
	}

	pub fn init(&mut self, index: u8) -> Result<(), Error> {
		let device = self.context.devices()?
			.iter()
			.filter(ThermalPrinter::printer_filter)
			.nth(index as usize)
			.expect("No printer found at index");

		self.handle = Some(device.open()?);
		let handle = self.handle.as_mut().unwrap();

		let config = device.active_config_descriptor()?;
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

		handle.claim_interface(interface.number())?;
		if self.context.supports_detach_kernel_driver() && handle.kernel_driver_active(interface.number())? {
			handle.detach_kernel_driver(interface.number())?;
		}

		// Reset printer
		let clear_command = [0x00; 200];
		self.write(&clear_command)?;
		let initialize_command = [0x1B, 0x40];
		self.write(&initialize_command)?;

		dbg!(self.get_status()?);

		Ok(())
	}

	fn get_status(&self) -> Result<Status::Response, Error> {
		let status_command = [0x1B, 0x69, 0x53];
		self.write(&status_command)?;
		self.read()
	}

	fn read(&self) -> Result<Status::Response, Error> {
		let handle = self.handle.as_ref().expect("Printer not initialized");

		const RECEIVE_SIZE: usize = 32;
		let mut response = [0; RECEIVE_SIZE];
		let bytes_read = handle.read_bulk(self.in_endpoint.unwrap(), &mut response, Duration::from_millis(500))?;

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
		let handle = self.handle.as_ref().expect("Printer not initialized");
		handle.write_bulk(self.out_endpoint.unwrap(), data, Duration::from_millis(500))?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use crate::printer::ThermalPrinter;
	#[test]
	fn printer_connect() {
		let context = libusb::Context::new().unwrap();
		let mut printer = ThermalPrinter::new(&context).unwrap();
		let available = printer.available_devices().unwrap();
		assert!(dbg!(available) > 0, "No printers found");
		printer.init(0).unwrap();
	}
}
