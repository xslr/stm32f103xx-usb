#![no_std]
#![no_main]

extern crate cortex_m;
#[macro_use]
extern crate cortex_m_rt as rt;
extern crate panic_semihosting;
extern crate stm32f103xx_hal as hal;
#[macro_use(block)]
extern crate nb;
extern crate usb_device;
extern crate stm32f103xx_usb;

use hal::prelude::*;
use hal::stm32f103xx;
use hal::timer::Timer;
use rt::ExceptionFrame;
use stm32f103xx_usb::UsbBus;

// Minimal CDC-ACM implementation
mod cdc_acm {
    use core::cell::RefCell;
    use core::cmp::min;
    use usb_device::{
        Result, UsbError,
        UsbBus,
        EndpointType, EndpointPair, EndpointIn, EndpointOut
    };
    use usb_device::class::{UsbClass, ControlOutResult, DescriptorWriter};
    use usb_device::control::*;

    const USB_CLASS_CDC: u8 = 0x02;
    const USB_CLASS_DATA: u8 = 0x0a;
    const CDC_SUBCLASS_ACM: u8 = 0x02;
    const CDC_PROTOCOL_AT: u8 = 0x01;

    const CS_INTERFACE: u8 = 0x24;
    const CDC_TYPE_HEADER: u8 = 0x00;
    const CDC_TYPE_CALL_MANAGEMENT: u8 = 0x01;
    const CDC_TYPE_ACM: u8 = 0x02;
    const CDC_TYPE_UNION: u8 = 0x06;

    const REQ_SET_LINE_CODING: u8 = 0x20;
    const REQ_SET_CONTROL_LINE_STATE: u8 = 0x22;

    struct Buf {
        buf: [u8; 64],
        len: usize,
    }

    pub struct SerialPort<'a, B: 'a + UsbBus> {
        comm_ep: EndpointIn<'a, B>,
        read_ep: EndpointOut<'a, B>,
        write_ep: EndpointIn<'a, B>,

        read_buf: RefCell<Buf>,
    }

    impl<'a, B: UsbBus> SerialPort<'a, B> {
        pub fn new(eps: (EndpointPair<'a, B>, EndpointPair<'a, B>))
            -> SerialPort<'a, B>
        {
            let (_, comm_ep) = eps.0.split(EndpointType::Interrupt, 8);
            let (read_ep, write_ep) = eps.1.split(EndpointType::Bulk, 64);

            SerialPort {
                comm_ep,
                read_ep,
                write_ep,
                read_buf: RefCell::new(Buf {
                    buf: [0; 64],
                    len: 0,
                }),
            }
        }

        pub fn write(&self, data: &[u8]) -> Result<usize> {
            match self.write_ep.write(data) {
                Ok(count) => Ok(count),
                Err(UsbError::Busy) => Ok(0),
                e => e,
            }
        }

        pub fn read(&self, data: &mut [u8]) -> Result<usize> {
            let mut buf = self.read_buf.borrow_mut();

            // Terrible buffering implementation for brevity's sake

            if buf.len == 0 {
                buf.len = match self.read_ep.read(&mut buf.buf) {
                    Ok(count) => count,
                    Err(UsbError::NoData) => return Ok(0),
                    e => return e,
                };
            }

            if buf.len == 0 {
                return Ok(0);
            }

            let count = min(data.len(), buf.len);

            &data[..count].copy_from_slice(&buf.buf[0..count]);

            buf.buf.rotate_left(count);
            buf.len -= count;

            Ok(count)
        }
    }

    impl<'a, B: UsbBus> UsbClass for SerialPort<'a, B> {
        fn reset(&self) -> Result<()> {
            self.comm_ep.configure()?;
            self.read_ep.configure()?;
            self.write_ep.configure()?;

            Ok(())
        }

        fn get_configuration_descriptors(&self, writer: &mut DescriptorWriter) -> Result<()> {
            // TODO: make a better DescriptorWriter to make it harder to make invalid descriptors
            let data_if = writer.interface(
                2,
                USB_CLASS_DATA,
                0x00,
                0x00)?;

            writer.endpoint(&self.write_ep)?;
            writer.endpoint(&self.read_ep)?;

            let comm_if = writer.interface(
                1,
                USB_CLASS_CDC,
                CDC_SUBCLASS_ACM,
                CDC_PROTOCOL_AT)?;

            writer.endpoint(&self.comm_ep)?;

            writer.write(
                CS_INTERFACE,
                &[CDC_TYPE_HEADER, 0x10, 0x01])?;

            writer.write(
                CS_INTERFACE,
                &[CDC_TYPE_CALL_MANAGEMENT, 0x00, data_if])?;

            writer.write(
                CS_INTERFACE,
                &[CDC_TYPE_ACM, 0x00])?;

            writer.write(
                CS_INTERFACE,
                &[CDC_TYPE_UNION, comm_if, data_if])?;

            Ok(())
        }

        fn control_out(&self, req: &Request, buf: &[u8]) -> ControlOutResult {
            let _ = buf;

            if req.request_type == RequestType::Class && req.recipient == Recipient::Interface {
                return match req.request {
                    REQ_SET_LINE_CODING => ControlOutResult::Ok,
                    REQ_SET_CONTROL_LINE_STATE => ControlOutResult::Ok,
                    _ => ControlOutResult::Ignore,
                };
            }

            ControlOutResult::Ignore
        }
    }
}

entry!(main);
fn main() -> ! {
    let cp = cortex_m::Peripherals::take().unwrap();
    let dp = stm32f103xx::Peripherals::take().unwrap();

    let mut flash = dp.FLASH.constrain();
    let mut rcc = dp.RCC.constrain();

    let clocks = rcc.cfgr
        .hse(8.mhz())
        .sysclk(48.mhz())
        .pclk1(24.mhz())
        .freeze(&mut flash.acr);

    assert!(clocks.usbclk_valid());

    let mut gpioa = dp.GPIOA.split(&mut rcc.apb2);
    let mut gpioc = dp.GPIOC.split(&mut rcc.apb2);

    let mut led = gpioc.pc13.into_push_pull_output(&mut gpioc.crh);

    let mut timer = Timer::syst(cp.SYST, 10.hz(), clocks);

    // hack to simulate USB reset
    {
        let mut pa12 = gpioa.pa12.into_push_pull_output(&mut gpioa.crh);
        pa12.set_low();
        for _ in 1..10 {
            block!(timer.wait()).unwrap();
        }
    }

    let usb_bus = UsbBus::usb(dp.USB, &mut rcc.apb1);
    let eps = usb_bus.endpoints().unwrap();

    let serial = cdc_acm::SerialPort::new((eps.ep1, eps.ep2));

    let usb_dev_info = usb_device::UsbDeviceInfo {
        manufacturer: "Fake company",
        product: "Serial port",
        serial_number: "TEST",
        ..usb_device::UsbDeviceInfo::new(0x5824, 0x27dd)
    };

    let usb_dev = usb_device::UsbDevice::new(&usb_bus, usb_dev_info, &[&serial]);

    loop {
        usb_dev.poll();

        if usb_dev.state() == usb_device::DeviceState::Configured {
            let mut buf = [0u8; 8];

            match serial.read(&mut buf) {
                Ok(count) if count > 0 => {
                    led.toggle();

                    // Echo back in upper case
                    for c in buf[0..count].iter_mut() {
                        if 0x61 <= *c && *c <= 0x7a {
                            *c &= !0x20;
                        }
                    }

                    serial.write(&buf[0..count]).ok();
                },
                _ => { },
            }
        }
    }
}

exception!(HardFault, hard_fault);
fn hard_fault(ef: &ExceptionFrame) -> ! {
    panic!("{:#?}", ef);
}

exception!(*, default_handler);
fn default_handler(irqn: i16) {
    panic!("Unhandled exception (IRQn = {})", irqn);
}
