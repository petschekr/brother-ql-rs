//! Everything to do with USB protocol for Brother QL printers

use std::time::Duration;
use std::thread;

pub mod constants;

error_chain! {
	foreign_links {
		USB(rusb::Error);
	}
}

#[allow(non_snake_case)]
pub mod status {
	//! A representation of the status message Brother QL printers use
	//!
	//! Includes:
	//! * Model name
	//! * Loaded media
	//! * Current operation
	//! * Any errors that have occurred
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

	#[derive(Debug, PartialEq)]
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

fn printer_filter<T: rusb::UsbContext>(device: &rusb::Device<T>) -> bool {
	let descriptor = device.device_descriptor().unwrap();
	if descriptor.vendor_id() == constants::VENDOR_ID && descriptor.product_id() == 0x2049 {
		eprintln!("You must disable Editor Lite mode on your QL-700 before you can print with it");
	}
	descriptor.vendor_id() == constants::VENDOR_ID && constants::printer_name_from_id(descriptor.product_id()).is_some()
}

/// Get a vector of all attached and supported Brother QL printers as USB devices from which `ThermalPrinter` structs can be initialized.
pub fn printers() -> Vec<rusb::Device<rusb::GlobalContext>> {
	rusb::DeviceList::new()
		.unwrap()
		.iter()
		.filter(printer_filter)
		.collect()
}

const RASTER_LINE_LENGTH: u8 = 90;

/// The primary interface for dealing with Brother QL printers. Handles all USB communication with the printer.
pub struct ThermalPrinter<T: rusb::UsbContext> {
	pub model: String,
	handle: rusb::DeviceHandle<T>,
	in_endpoint: u8,
	out_endpoint: u8,
}
impl<T: rusb::UsbContext> ThermalPrinter<T> {
	/// Create a new `ThermalPrinter` instance using a `rusb` USB device handle.
	///
	/// Obtain list of connected device handles by calling `printers()`.
	pub fn new(device: rusb::Device<T>) -> Result<Self> {
		let mut handle = device.open()?;
		let mut in_endpoint: Option<u8> = None;
		let mut out_endpoint: Option<u8> = None;

		let config = device.active_config_descriptor()?;
		let interface = config.interfaces().next().chain_err(|| "Brother QL printers should have exactly one interface")?;
		let interface_descriptor = interface.descriptors().next().chain_err(|| "Brother QL printers should have exactly one interface descriptor")?;
		for endpoint in interface_descriptor.endpoint_descriptors() {
			if endpoint.transfer_type() != rusb::TransferType::Bulk {
				bail!("Brother QL printers are defined as using only bulk endpoint communication");
			}
			match endpoint.direction() {
				rusb::Direction::In  => in_endpoint  = Some(endpoint.address()),
				rusb::Direction::Out => out_endpoint = Some(endpoint.address()),
			}
		}
		if in_endpoint.is_none() || out_endpoint.is_none() {
			bail!("Input or output endpoint not found");
		}

		handle.claim_interface(interface.number())?;
		if let Ok(kd_active) = handle.kernel_driver_active(interface.number()) {
			if kd_active {
				handle.detach_kernel_driver(interface.number())?;
			}
		}

		let mut printer = ThermalPrinter {
			model: String::new(),
			handle,
			in_endpoint: in_endpoint.unwrap(),
			out_endpoint: out_endpoint.unwrap(),
		};

		// Reset printer
		let clear_command = [0x00; 200];
		ThermalPrinter::write(&printer, &clear_command)?;
		let initialize_command = [0x1B, 0x40];
		ThermalPrinter::write(&printer, &initialize_command)?;

		let status = ThermalPrinter::get_status(&printer)?;
		printer.model = status.model.to_string();
		Ok(printer)
	}

	/// Sends raster lines to the USB printer, begins printing, and immediately returns
	///
	/// Images on the label tape are comprised of bits representing either black (`1`) or white (`0`). They are
	/// arranged in lines of a static width that corresponds to the width of the printer's thermal print head.
	///
	/// **Note:** the raster line width does not change for label media of different sizes. This means the
	/// printer can print out-of-bounds and even print on parts of the label not originally intended to
	/// contain content. Your rasterizer will have to figure out, given a media type, which parts of the
	/// image will appear on the media and resize or shift margins and content accordingly.
	pub fn print(&self, raster_lines: Vec<[u8; RASTER_LINE_LENGTH as usize]>) -> Result<status::Response> {
		let status = self.get_status()?;

		let mode_command = [0x1B, 0x69, 0x61, 1];
		self.write(&mode_command)?;

		const VALID_FLAGS: u8 = 0x80 | 0x02 | 0x04 | 0x08 | 0x40; // Everything enabled
		let media_type: u8 = match status.media.media_type {
			status::MediaType::ContinuousTape => 0x0A,
			status::MediaType::DieCutLabels => 0x0B,
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
	/// Same as `print()` but will not return until the printer reports that it has finished printing.
	pub fn print_blocking(&self, raster_lines: Vec<[u8; RASTER_LINE_LENGTH as usize]>) -> Result<()> {
		self.print(raster_lines)?;
		loop {
			match self.read() {
				Ok(ref response) if response.status_type == status::StatusType::PrintingCompleted => break,
				_ => thread::sleep(Duration::from_millis(50)),
			}
		}
		Ok(())
	}

	/// Get the currently loaded label size.
	pub fn current_label(&self) -> Result<constants::Label> {
		let media = self.get_status()?.media;
		constants::label_data(media.width, match media.length {
			0 => None,
			_ => Some(media.length)
		}).ok_or("Unknown media loaded in printer".into())
	}

	/// Get the current status of the printer including possible errors, media type, and model name.
	pub fn get_status(&self) -> Result<status::Response> {
		let status_command = [0x1B, 0x69, 0x53];
		self.write(&status_command)?;
		self.read()
	}

	fn read(&self) -> Result<status::Response> {
		const RECEIVE_SIZE: usize = 32;
		let mut response = [0; RECEIVE_SIZE];
		let bytes_read = self.handle.read_bulk(self.in_endpoint, &mut response, Duration::from_millis(500))?;

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
			0x0A => status::MediaType::ContinuousTape,
			0x0B => status::MediaType::DieCutLabels,
			_    => status::MediaType::None,
		};

		let status_type = match response[18] {
			0x00 => status::StatusType::ReplyToStatusRequest,
			0x01 => status::StatusType::PrintingCompleted,
			0x02 => status::StatusType::ErrorOccurred,
			0x05 => status::StatusType::Notification,
			0x06 => status::StatusType::PhaseChange,
			// Will never occur
			_ => status::StatusType::Notification
		};

		Ok(status::Response {
			model,
			status_type,
			errors,
			media: status::Media {
				media_type,
				width,
				length,
			}
		})
	}

	fn write(&self, data: &[u8]) -> Result<()> {
		self.handle.write_bulk(self.out_endpoint, data, Duration::from_millis(500))?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use crate::printer::{ printers, ThermalPrinter };
	#[test]
	fn connect() {
		let printer_list = printers();
		assert!(printer_list.len() > 0, "No printers found");
		let mut printer = ThermalPrinter::new(printer_list.into_iter().next().unwrap()).unwrap();
		printer.init().unwrap();
	}

	use std::path::PathBuf;
    #[test]
	#[ignore]
    fn print() {
		let printer_list = printers();
		assert!(printer_list.len() > 0, "No printers found");
		let mut printer = ThermalPrinter::new(printer_list.into_iter().next().unwrap()).unwrap();
		let label = printer.init().unwrap().media.to_label();

        let mut rasterizer = crate::text::TextRasterizer::new(
            label,
            PathBuf::from("./Space Mono Bold.ttf")
        );
        rasterizer.set_second_row_image(PathBuf::from("./logos/BuildGT Mono.png"));
        let lines = rasterizer.rasterize(
            "Ryan Petschek",
            Some("Computer Science"),
			1.2,
			false
        );

		dbg!(printer.print(lines).unwrap());
    }
}
