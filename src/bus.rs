use core::cell::RefCell;
use core::mem;
use usb_device::{Result, UsbError};
use usb_device::bus::{UsbBusWrapper, PollResult};
use usb_device::endpoint::{EndpointDirection, EndpointType, EndpointAddress};
use usb_device::utils::{FreezableRefCell, AtomicMutex};
//use bare_metal::Mutex;
use cortex_m::asm::delay;
use cortex_m::interrupt;
use stm32f103xx::USB;
use stm32f103xx_hal::prelude::*;
use stm32f103xx_hal::rcc;
use stm32f103xx_hal::gpio::{self, gpioa};
use endpoint::{NUM_ENDPOINTS, Endpoint, EndpointStatus, calculate_count_rx};

struct Reset {
    delay: u32,
    pin: RefCell<gpioa::PA12<gpio::Output<gpio::PushPull>>>,
}

/// USB peripheral driver for STM32F103 microcontrollers.
pub struct UsbBus {
    regs: AtomicMutex<USB>,
    endpoints: [Endpoint; NUM_ENDPOINTS],
    next_ep_mem: usize,
    max_endpoint: usize,
    reset: FreezableRefCell<Option<Reset>>,
}

impl UsbBus {
    /// Constructs a new USB peripheral driver.
    pub fn usb(regs: USB, apb1: &mut rcc::APB1) -> UsbBusWrapper<Self> {
        // TODO: apb1.enr is not public, figure out how this should really interact with the HAL
        // crate

        interrupt::free(|_| {
            apb1.enr().modify(|_, w| w.usben().enabled());
        });

        let bus = UsbBus {
            regs: AtomicMutex::new(regs),
            next_ep_mem: Endpoint::MEM_START,
            max_endpoint: 0,
            endpoints: unsafe {
                let mut endpoints: [Endpoint; NUM_ENDPOINTS] = mem::uninitialized();

                for i in 0..NUM_ENDPOINTS {
                    endpoints[i] = Endpoint::new(i as u8);
                }

                endpoints
            },
            reset: FreezableRefCell::default(),
        };

        UsbBusWrapper::new(bus)
    }

    /// Enables the `reset` method.
    pub fn enable_reset<M>(&mut self,
        clocks: &rcc::Clocks, crh: &mut gpioa::CRH, pa12: gpioa::PA12<M>)
    {
        *self.reset.borrow_mut() = Some(Reset {
            delay: clocks.sysclk().0,
            pin: RefCell::new(pa12.into_push_pull_output(crh)),
        });
    }

    fn alloc_ep_mem(next_ep_mem: &mut usize, size: usize) -> Result<usize> {
        assert!(size & 1 == 0);

        let addr = *next_ep_mem;
        if addr + size > Endpoint::MEM_SIZE {
            return Err(UsbError::SizeOverflow);
        }

        *next_ep_mem += size;

        Ok(addr)
    }
}

impl ::usb_device::bus::UsbBus for UsbBus {
    fn alloc_ep(
        &mut self,
        ep_dir: EndpointDirection,
        ep_addr: Option<EndpointAddress>,
        ep_type: EndpointType,
        max_packet_size: u16,
        _interval: u8) -> Result<EndpointAddress>
    {
        for index in ep_addr.map(|a| a.index()..a.index()+1).unwrap_or(1..NUM_ENDPOINTS) {
            let ep = &mut self.endpoints[index];

            match ep.ep_type() {
                None => { ep.set_ep_type(ep_type); },
                Some(t) if t != ep_type => { continue; },
                _ => { },
            };

            match ep_dir {
                EndpointDirection::Out if !ep.is_out_buf_set() => {
                    let (out_size, bits) = calculate_count_rx(max_packet_size as usize)?;

                    let addr = Self::alloc_ep_mem(&mut self.next_ep_mem, out_size)?;

                    ep.set_out_buf(addr, (out_size, bits));

                    return Ok(EndpointAddress::from_parts(index, ep_dir));
                },
                EndpointDirection::In if !ep.is_in_buf_set() => {
                    let addr = Self::alloc_ep_mem(&mut self.next_ep_mem, max_packet_size as usize)?;

                    ep.set_in_buf(addr, max_packet_size as usize);

                    return Ok(EndpointAddress::from_parts(index, ep_dir));
                }
                _ => { }
            }
        }

        Err(UsbError::EndpointOverflow)
    }

    fn enable(&mut self) {
        self.reset.freeze();

        let mut max = 0;
        for (index, ep) in self.endpoints.iter().enumerate() {
            if ep.is_out_buf_set() || ep.is_in_buf_set() {
                max = index;
            }
        }

        self.max_endpoint = max;

        interrupt::free(|_| {
            let regs = self.regs.try_lock().unwrap();

            regs.cntr.modify(|_, w| w.pdwn().clear_bit());

            // There is a chip specific startup delay. For STM32F103xx it's 1µs and this should wait for
            // at least that long.
            delay(72);

            regs.btable.modify(|_, w| unsafe { w.btable().bits(0) });
            regs.cntr.modify(|_, w| w.fres().clear_bit());
            regs.istr.modify(|_, w| unsafe { w.bits(0) });
        });
    }

    fn reset(&self) {
        interrupt::free(|cs| {
            let regs = self.regs.try_lock().unwrap();

            regs.istr.modify(|_, w| unsafe { w.bits(0) });
            regs.daddr.modify(|_, w| unsafe { w.ef().set_bit().add().bits(0) });

            for ep in self.endpoints.iter() {
                ep.configure(cs);
            }
        });
    }

    fn set_device_address(&self, addr: u8) {
        interrupt::free(|_| {
            self.regs.try_lock().unwrap().daddr.modify(|_, w| unsafe { w.add().bits(addr as u8) });
        });
    }

    fn poll(&self) -> PollResult {
        let mut guard = self.regs.try_lock();

        let regs = match guard {
            Some(ref mut r) => r,
            // re-entrant call, any interrupts will be handled by the already-running call or the
            // next call
            None => { return PollResult::None; }
        };

        let istr = regs.istr.read();

        if istr.wkup().bit_is_set() {
            regs.istr.modify(|_, w| w.wkup().clear_bit());

            let fnr = regs.fnr.read();
            //let bits = (fnr.rxdp().bit_is_set() as u8) << 1 | (fnr.rxdm().bit_is_set() as u8);

            match (fnr.rxdp().bit_is_set(), fnr.rxdm().bit_is_set()) {
                (false, false) | (false, true) => {
                    PollResult::Resume
                },
                _ => {
                    // Spurious wakeup event caused by noise
                    PollResult::Suspend
                }
            }
        } else if istr.reset().bit_is_set() {
            regs.istr.modify(|_, w| w.reset().clear_bit());

            PollResult::Reset
        } else if istr.susp().bit_is_set() {
            regs.istr.modify(|_, w| w.susp().clear_bit());

            PollResult::Suspend
        } else if istr.ctr().bit_is_set() {
            let mut ep_out = 0;
            let mut ep_in_complete = 0;
            let mut ep_setup = 0;
            let mut bit = 1;

            for ep in &self.endpoints[0..=self.max_endpoint] {
                let v = ep.read_reg();

                if v.ctr_rx().bit_is_set() {
                    ep_out |= bit;

                    if v.setup().bit_is_set() {
                        ep_setup |= bit;
                    }
                }

                if v.ctr_tx().bit_is_set() {
                    ep_in_complete |= bit;

                    interrupt::free(|cs| {
                        ep.clear_ctr_tx(cs);
                    });
                }

                bit <<= 1;
            }

            PollResult::Data { ep_out, ep_in_complete, ep_setup }
        } else {
            PollResult::None
        }
    }

    fn write(&self, ep_addr: EndpointAddress, buf: &[u8]) -> Result<usize> {
        if !ep_addr.is_in() {
            return Err(UsbError::InvalidEndpoint);
        }

        self.endpoints[ep_addr.index()].write(buf)
    }

    fn read(&self, ep_addr: EndpointAddress, buf: &mut [u8]) -> Result<usize> {
        if !ep_addr.is_out() {
            return Err(UsbError::InvalidEndpoint);
        }

        self.endpoints[ep_addr.index()].read(buf)
    }

    fn set_stalled(&self, ep_addr: EndpointAddress, stalled: bool) {
        interrupt::free(|cs| {
            if self.is_stalled(ep_addr) == stalled {
                return
            }

            let ep = &self.endpoints[ep_addr.index()];

            match (stalled, ep_addr.direction()) {
                (true, EndpointDirection::In) => ep.set_stat_tx(cs, EndpointStatus::Stall),
                (true, EndpointDirection::Out) => ep.set_stat_rx(cs, EndpointStatus::Stall),
                (false, EndpointDirection::In) => ep.set_stat_tx(cs, EndpointStatus::Nak),
                (false, EndpointDirection::Out) => ep.set_stat_rx(cs, EndpointStatus::Valid),
            };
        });
    }

    fn is_stalled(&self, ep_addr: EndpointAddress) -> bool {
        let ep = &self.endpoints[ep_addr.index()];
        let reg_v = ep.read_reg();

        let status = match ep_addr.direction() {
            EndpointDirection::In => reg_v.stat_tx().bits(),
            EndpointDirection::Out => reg_v.stat_rx().bits(),
        };

        status == (EndpointStatus::Stall as u8)
    }

    fn suspend(&self) {
        interrupt::free(|_| {
            self.regs.try_lock().unwrap().cntr.modify(|_, w| w
                .fsusp().set_bit()
                .lpmode().set_bit());
        });
    }

    fn resume(&self) {
        interrupt::free(|_| {
            self.regs.try_lock().unwrap().cntr.modify(|_, w| w
                .fsusp().clear_bit()
                .lpmode().clear_bit());
        });
    }

    fn force_reset(&self) -> Result<()> {
        interrupt::free(|_| {
            let regs = self.regs.try_lock().unwrap();

            match *self.reset.borrow() {
                Some(ref reset) => {
                    let pdwn = regs.cntr.read().pdwn().bit_is_set();
                    regs.cntr.modify(|_, w| w.pdwn().set_bit());

                    reset.pin.borrow_mut().set_low();
                    delay(reset.delay);

                    regs.cntr.modify(|_, w| w.pdwn().bit(pdwn));

                    Ok(())
                },
                None => Err(UsbError::Unsupported),
            }
        })
    }
}
