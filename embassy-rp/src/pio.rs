use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin as FuturePin;
use core::sync::atomic::{compiler_fence, Ordering};
use core::task::{Context, Poll};

use embassy_cortex_m::interrupt::{Interrupt, InterruptExt};
use embassy_hal_common::{Peripheral, PeripheralRef};
use embassy_sync::waitqueue::AtomicWaker;

use crate::dma::{Channel, Transfer, Word};
use crate::gpio::sealed::Pin as SealedPin;
use crate::gpio::{Drive, Pin, Pull, SlewRate};
use crate::pac::dma::vals::TreqSel;
use crate::pio::sealed::PioInstance as _;
use crate::{interrupt, pac, peripherals, RegExt};

struct Wakers([AtomicWaker; 12]);

impl Wakers {
    #[inline(always)]
    fn fifo_in(&self) -> &[AtomicWaker] {
        &self.0[0..4]
    }
    #[inline(always)]
    fn fifo_out(&self) -> &[AtomicWaker] {
        &self.0[4..8]
    }
    #[inline(always)]
    fn irq(&self) -> &[AtomicWaker] {
        &self.0[8..12]
    }
}

const NEW_AW: AtomicWaker = AtomicWaker::new();
const PIO_WAKERS_INIT: Wakers = Wakers([NEW_AW; 12]);
static WAKERS: [Wakers; 2] = [PIO_WAKERS_INIT; 2];

pub enum FifoJoin {
    /// Both TX and RX fifo is enabled
    Duplex,
    /// Rx fifo twice as deep. TX fifo disabled
    RxOnly,
    /// Tx fifo twice as deep. RX fifo disabled
    TxOnly,
}

#[derive(PartialEq)]
pub enum ShiftDirection {
    Right = 1,
    Left = 0,
}

const RXNEMPTY_MASK: u32 = 1 << 0;
const TXNFULL_MASK: u32 = 1 << 4;
const SMIRQ_MASK: u32 = 1 << 8;

#[interrupt]
unsafe fn PIO0_IRQ_0() {
    use crate::pac;
    let ints = pac::PIO0.irqs(0).ints().read().0;
    for bit in 0..12 {
        if ints & (1 << bit) != 0 {
            WAKERS[0].0[bit].wake();
        }
    }
    pac::PIO0.irqs(0).inte().write_clear(|m| m.0 = ints);
}

#[interrupt]
unsafe fn PIO1_IRQ_0() {
    use crate::pac;
    let ints = pac::PIO1.irqs(0).ints().read().0;
    for bit in 0..12 {
        if ints & (1 << bit) != 0 {
            WAKERS[1].0[bit].wake();
        }
    }
    pac::PIO1.irqs(0).inte().write_clear(|m| m.0 = ints);
}

pub(crate) unsafe fn init() {
    let irq = interrupt::PIO0_IRQ_0::steal();
    irq.disable();
    irq.set_priority(interrupt::Priority::P3);
    pac::PIO0.irqs(0).inte().write(|m| m.0 = 0);
    irq.enable();

    let irq = interrupt::PIO1_IRQ_0::steal();
    irq.disable();
    irq.set_priority(interrupt::Priority::P3);
    pac::PIO1.irqs(0).inte().write(|m| m.0 = 0);
    irq.enable();
}

/// Future that waits for TX-FIFO to become writable
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct FifoOutFuture<'a, PIO: PioInstance, SM: PioStateMachine + Unpin> {
    sm: &'a mut SM,
    pio: PhantomData<PIO>,
    value: u32,
}

impl<'a, PIO: PioInstance, SM: PioStateMachine + Unpin> FifoOutFuture<'a, PIO, SM> {
    pub fn new(sm: &'a mut SM, value: u32) -> Self {
        FifoOutFuture {
            sm,
            pio: PhantomData::default(),
            value,
        }
    }
}

impl<'d, PIO: PioInstance, SM: PioStateMachine + Unpin> Future for FifoOutFuture<'d, PIO, SM> {
    type Output = ();
    fn poll(self: FuturePin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //debug!("Poll {},{}", PIO::PIO_NO, SM);
        let value = self.value;
        if self.get_mut().sm.try_push_tx(value) {
            Poll::Ready(())
        } else {
            WAKERS[PIO::PIO_NO as usize].fifo_out()[SM::SM as usize].register(cx.waker());
            unsafe {
                PIO::PIO.irqs(0).inte().write_set(|m| {
                    m.0 = TXNFULL_MASK << SM::SM;
                });
            }
            // debug!("Pending");
            Poll::Pending
        }
    }
}

impl<'d, PIO: PioInstance, SM: PioStateMachine + Unpin> Drop for FifoOutFuture<'d, PIO, SM> {
    fn drop(&mut self) {
        unsafe {
            PIO::PIO.irqs(0).inte().write_clear(|m| {
                m.0 = TXNFULL_MASK << SM::SM;
            });
        }
    }
}

/// Future that waits for RX-FIFO to become readable
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct FifoInFuture<'a, PIO: PioInstance, SM: PioStateMachine> {
    sm: &'a mut SM,
    pio: PhantomData<PIO>,
}

impl<'a, PIO: PioInstance, SM: PioStateMachine> FifoInFuture<'a, PIO, SM> {
    pub fn new(sm: &'a mut SM) -> Self {
        FifoInFuture {
            sm,
            pio: PhantomData::default(),
        }
    }
}

impl<'d, PIO: PioInstance, SM: PioStateMachine> Future for FifoInFuture<'d, PIO, SM> {
    type Output = u32;
    fn poll(mut self: FuturePin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //debug!("Poll {},{}", PIO::PIO_NO, SM);
        if let Some(v) = self.sm.try_pull_rx() {
            Poll::Ready(v)
        } else {
            WAKERS[PIO::PIO_NO as usize].fifo_in()[SM::SM].register(cx.waker());
            unsafe {
                PIO::PIO.irqs(0).inte().write_set(|m| {
                    m.0 = RXNEMPTY_MASK << SM::SM;
                });
            }
            //debug!("Pending");
            Poll::Pending
        }
    }
}

impl<'d, PIO: PioInstance, SM: PioStateMachine> Drop for FifoInFuture<'d, PIO, SM> {
    fn drop(&mut self) {
        unsafe {
            PIO::PIO.irqs(0).inte().write_clear(|m| {
                m.0 = RXNEMPTY_MASK << SM::SM;
            });
        }
    }
}

/// Future that waits for IRQ
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct IrqFuture<PIO: PioInstance> {
    pio: PhantomData<PIO>,
    irq_no: u8,
}

impl<'a, PIO: PioInstance> IrqFuture<PIO> {
    pub fn new(irq_no: u8) -> Self {
        IrqFuture {
            pio: PhantomData::default(),
            irq_no,
        }
    }
}

impl<'d, PIO: PioInstance> Future for IrqFuture<PIO> {
    type Output = ();
    fn poll(self: FuturePin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //debug!("Poll {},{}", PIO::PIO_NO, SM);

        // Check if IRQ flag is already set
        if critical_section::with(|_| unsafe {
            let irq_flags = PIO::PIO.irq();
            if irq_flags.read().0 & (1 << self.irq_no) != 0 {
                irq_flags.write(|m| {
                    m.0 = 1 << self.irq_no;
                });
                true
            } else {
                false
            }
        }) {
            return Poll::Ready(());
        }

        WAKERS[PIO::PIO_NO as usize].irq()[self.irq_no as usize].register(cx.waker());
        unsafe {
            PIO::PIO.irqs(0).inte().write_set(|m| {
                m.0 = SMIRQ_MASK << self.irq_no;
            });
        }
        Poll::Pending
    }
}

impl<'d, PIO: PioInstance> Drop for IrqFuture<PIO> {
    fn drop(&mut self) {
        unsafe {
            PIO::PIO.irqs(0).inte().write_clear(|m| {
                m.0 = SMIRQ_MASK << self.irq_no;
            });
        }
    }
}

pub struct PioPin<PIO: PioInstance> {
    pin_bank: u8,
    pio: PhantomData<PIO>,
}

impl<PIO: PioInstance> PioPin<PIO> {
    /// Set the pin's drive strength.
    #[inline]
    pub fn set_drive_strength(&mut self, strength: Drive) {
        unsafe {
            self.pad_ctrl().modify(|w| {
                w.set_drive(match strength {
                    Drive::_2mA => pac::pads::vals::Drive::_2MA,
                    Drive::_4mA => pac::pads::vals::Drive::_4MA,
                    Drive::_8mA => pac::pads::vals::Drive::_8MA,
                    Drive::_12mA => pac::pads::vals::Drive::_12MA,
                });
            });
        }
    }

    // Set the pin's slew rate.
    #[inline]
    pub fn set_slew_rate(&mut self, slew_rate: SlewRate) {
        unsafe {
            self.pad_ctrl().modify(|w| {
                w.set_slewfast(slew_rate == SlewRate::Fast);
            });
        }
    }

    /// Set the pin's pull.
    #[inline]
    pub fn set_pull(&mut self, pull: Pull) {
        unsafe {
            self.pad_ctrl().modify(|w| {
                w.set_pue(pull == Pull::Up);
                w.set_pde(pull == Pull::Down);
            });
        }
    }

    /// Set the pin's schmitt trigger.
    #[inline]
    pub fn set_schmitt(&mut self, enable: bool) {
        unsafe {
            self.pad_ctrl().modify(|w| {
                w.set_schmitt(enable);
            });
        }
    }

    pub fn set_input_sync_bypass<'a>(&mut self, bypass: bool) {
        let mask = 1 << self.pin();
        unsafe {
            if bypass {
                PIO::PIO.input_sync_bypass().write_set(|w| *w = mask);
            } else {
                PIO::PIO.input_sync_bypass().write_clear(|w| *w = mask);
            }
        }
    }

    pub fn pin(&self) -> u8 {
        self._pin()
    }
}

impl<PIO: PioInstance> SealedPin for PioPin<PIO> {
    fn pin_bank(&self) -> u8 {
        self.pin_bank
    }
}

pub struct PioStateMachineInstance<'d, PIO: PioInstance, const SM: usize> {
    pio: PhantomData<&'d PIO>,
}

impl<'d, PIO: PioInstance, const SM: usize> sealed::PioStateMachine for PioStateMachineInstance<'d, PIO, SM> {
    type Pio = PIO;
    const SM: usize = SM;
}
impl<'d, PIO: PioInstance, const SM: usize> PioStateMachine for PioStateMachineInstance<'d, PIO, SM> {}

pub trait PioStateMachine: sealed::PioStateMachine + Sized + Unpin {
    fn pio_no(&self) -> u8 {
        Self::Pio::PIO_NO
    }

    fn sm_no(&self) -> u8 {
        Self::SM as u8
    }

    fn restart(&mut self) {
        let mask = 1u8 << Self::SM;
        unsafe {
            Self::Pio::PIO.ctrl().write_set(|w| w.set_sm_restart(mask));
        }
    }
    fn set_enable(&mut self, enable: bool) {
        let mask = 1u8 << Self::SM;
        unsafe {
            if enable {
                Self::Pio::PIO.ctrl().write_set(|w| w.set_sm_enable(mask));
            } else {
                Self::Pio::PIO.ctrl().write_clear(|w| w.set_sm_enable(mask));
            }
        }
    }

    fn is_enabled(&self) -> bool {
        unsafe { Self::Pio::PIO.ctrl().read().sm_enable() & (1u8 << Self::SM) != 0 }
    }

    fn is_tx_empty(&self) -> bool {
        unsafe { Self::Pio::PIO.fstat().read().txempty() & (1u8 << Self::SM) != 0 }
    }
    fn is_tx_full(&self) -> bool {
        unsafe { Self::Pio::PIO.fstat().read().txfull() & (1u8 << Self::SM) != 0 }
    }

    fn is_rx_empty(&self) -> bool {
        unsafe { Self::Pio::PIO.fstat().read().rxempty() & (1u8 << Self::SM) != 0 }
    }
    fn is_rx_full(&self) -> bool {
        unsafe { Self::Pio::PIO.fstat().read().rxfull() & (1u8 << Self::SM) != 0 }
    }

    fn tx_level(&self) -> u8 {
        unsafe {
            let flevel = Self::Pio::PIO.flevel().read().0;
            (flevel >> (Self::SM * 8)) as u8 & 0x0f
        }
    }

    fn rx_level(&self) -> u8 {
        unsafe {
            let flevel = Self::Pio::PIO.flevel().read().0;
            (flevel >> (Self::SM * 8 + 4)) as u8 & 0x0f
        }
    }

    fn push_tx(&mut self, v: u32) {
        unsafe {
            Self::Pio::PIO.txf(Self::SM).write_value(v);
        }
    }

    fn try_push_tx(&mut self, v: u32) -> bool {
        if self.is_tx_full() {
            return false;
        }
        self.push_tx(v);
        true
    }

    fn pull_rx(&mut self) -> u32 {
        unsafe { Self::Pio::PIO.rxf(Self::SM).read() }
    }

    fn try_pull_rx(&mut self) -> Option<u32> {
        if self.is_rx_empty() {
            return None;
        }
        Some(self.pull_rx())
    }

    fn set_clkdiv(&mut self, div_x_256: u32) {
        unsafe {
            Self::this_sm().clkdiv().write(|w| w.0 = div_x_256 << 8);
        }
    }

    fn get_clkdiv(&self) -> u32 {
        unsafe { Self::this_sm().clkdiv().read().0 >> 8 }
    }

    fn clkdiv_restart(&mut self) {
        let mask = 1u8 << Self::SM;
        unsafe {
            Self::Pio::PIO.ctrl().write_set(|w| w.set_clkdiv_restart(mask));
        }
    }

    fn set_side_enable(&self, enable: bool) {
        unsafe {
            Self::this_sm().execctrl().modify(|w| w.set_side_en(enable));
        }
    }

    fn is_side_enabled(&self) -> bool {
        unsafe { Self::this_sm().execctrl().read().side_en() }
    }

    fn set_side_pindir(&mut self, pindir: bool) {
        unsafe {
            Self::this_sm().execctrl().modify(|w| w.set_side_pindir(pindir));
        }
    }

    fn is_side_pindir(&self) -> bool {
        unsafe { Self::this_sm().execctrl().read().side_pindir() }
    }

    fn set_jmp_pin(&mut self, pin: u8) {
        unsafe {
            Self::this_sm().execctrl().modify(|w| w.set_jmp_pin(pin));
        }
    }

    fn get_jmp_pin(&mut self) -> u8 {
        unsafe { Self::this_sm().execctrl().read().jmp_pin() }
    }

    fn set_wrap(&self, source: u8, target: u8) {
        unsafe {
            Self::this_sm().execctrl().modify(|w| {
                w.set_wrap_top(source);
                w.set_wrap_bottom(target)
            });
        }
    }

    /// Get wrapping addresses. Returns (source, target).
    fn get_wrap(&self) -> (u8, u8) {
        unsafe {
            let r = Self::this_sm().execctrl().read();
            (r.wrap_top(), r.wrap_bottom())
        }
    }

    fn set_fifo_join(&mut self, join: FifoJoin) {
        let (rx, tx) = match join {
            FifoJoin::Duplex => (false, false),
            FifoJoin::RxOnly => (true, false),
            FifoJoin::TxOnly => (false, true),
        };
        unsafe {
            Self::this_sm().shiftctrl().modify(|w| {
                w.set_fjoin_rx(rx);
                w.set_fjoin_tx(tx)
            });
        }
    }
    fn get_fifo_join(&self) -> FifoJoin {
        unsafe {
            let r = Self::this_sm().shiftctrl().read();
            // Ignores the invalid state when both bits are set
            if r.fjoin_rx() {
                FifoJoin::RxOnly
            } else if r.fjoin_tx() {
                FifoJoin::TxOnly
            } else {
                FifoJoin::Duplex
            }
        }
    }

    fn clear_fifos(&mut self) {
        // Toggle FJOIN_RX to flush FIFOs
        unsafe {
            let shiftctrl = Self::this_sm().shiftctrl();
            shiftctrl.modify(|w| {
                w.set_fjoin_rx(!w.fjoin_rx());
            });
            shiftctrl.modify(|w| {
                w.set_fjoin_rx(!w.fjoin_rx());
            });
        }
    }

    fn set_pull_threshold(&mut self, threshold: u8) {
        unsafe {
            Self::this_sm().shiftctrl().modify(|w| w.set_pull_thresh(threshold));
        }
    }

    fn get_pull_threshold(&self) -> u8 {
        unsafe { Self::this_sm().shiftctrl().read().pull_thresh() }
    }
    fn set_push_threshold(&mut self, threshold: u8) {
        unsafe {
            Self::this_sm().shiftctrl().modify(|w| w.set_push_thresh(threshold));
        }
    }

    fn get_push_threshold(&self) -> u8 {
        unsafe { Self::this_sm().shiftctrl().read().push_thresh() }
    }

    fn set_out_shift_dir(&mut self, dir: ShiftDirection) {
        unsafe {
            Self::this_sm()
                .shiftctrl()
                .modify(|w| w.set_out_shiftdir(dir == ShiftDirection::Right));
        }
    }
    fn get_out_shiftdir(&self) -> ShiftDirection {
        unsafe {
            if Self::this_sm().shiftctrl().read().out_shiftdir() {
                ShiftDirection::Right
            } else {
                ShiftDirection::Left
            }
        }
    }

    fn set_in_shift_dir(&mut self, dir: ShiftDirection) {
        unsafe {
            Self::this_sm()
                .shiftctrl()
                .modify(|w| w.set_in_shiftdir(dir == ShiftDirection::Right));
        }
    }
    fn get_in_shiftdir(&self) -> ShiftDirection {
        unsafe {
            if Self::this_sm().shiftctrl().read().in_shiftdir() {
                ShiftDirection::Right
            } else {
                ShiftDirection::Left
            }
        }
    }

    fn set_autopull(&mut self, auto: bool) {
        unsafe {
            Self::this_sm().shiftctrl().modify(|w| w.set_autopull(auto));
        }
    }

    fn is_autopull(&self) -> bool {
        unsafe { Self::this_sm().shiftctrl().read().autopull() }
    }

    fn set_autopush(&mut self, auto: bool) {
        unsafe {
            Self::this_sm().shiftctrl().modify(|w| w.set_autopush(auto));
        }
    }

    fn is_autopush(&self) -> bool {
        unsafe { Self::this_sm().shiftctrl().read().autopush() }
    }

    fn get_addr(&self) -> u8 {
        unsafe { Self::this_sm().addr().read().addr() }
    }
    fn set_sideset_count(&mut self, count: u8) {
        unsafe {
            Self::this_sm().pinctrl().modify(|w| w.set_sideset_count(count));
        }
    }

    fn get_sideset_count(&self) -> u8 {
        unsafe { Self::this_sm().pinctrl().read().sideset_count() }
    }

    fn set_sideset_base_pin(&mut self, base_pin: &PioPin<Self::Pio>) {
        unsafe {
            Self::this_sm().pinctrl().modify(|w| w.set_sideset_base(base_pin.pin()));
        }
    }

    fn get_sideset_base(&self) -> u8 {
        unsafe {
            let r = Self::this_sm().pinctrl().read();
            r.sideset_base()
        }
    }

    /// Set the range of out pins affected by a set instruction.
    fn set_set_range(&mut self, base: u8, count: u8) {
        assert!(base + count < 32);
        unsafe {
            Self::this_sm().pinctrl().modify(|w| {
                w.set_set_base(base);
                w.set_set_count(count)
            });
        }
    }

    /// Get the range of out pins affected by a set instruction. Returns (base, count).
    fn get_set_range(&self) -> (u8, u8) {
        unsafe {
            let r = Self::this_sm().pinctrl().read();
            (r.set_base(), r.set_count())
        }
    }

    fn set_in_base_pin(&mut self, base: &PioPin<Self::Pio>) {
        unsafe {
            Self::this_sm().pinctrl().modify(|w| w.set_in_base(base.pin()));
        }
    }

    fn get_in_base(&self) -> u8 {
        unsafe {
            let r = Self::this_sm().pinctrl().read();
            r.in_base()
        }
    }

    fn set_out_range(&mut self, base: u8, count: u8) {
        assert!(base + count < 32);
        unsafe {
            Self::this_sm().pinctrl().modify(|w| {
                w.set_out_base(base);
                w.set_out_count(count)
            });
        }
    }

    /// Get the range of out pins affected by a set instruction. Returns (base, count).
    fn get_out_range(&self) -> (u8, u8) {
        unsafe {
            let r = Self::this_sm().pinctrl().read();
            (r.out_base(), r.out_count())
        }
    }

    fn set_out_pins<'a, 'b: 'a>(&'a mut self, pins: &'b [&PioPin<Self::Pio>]) {
        let count = pins.len();
        assert!(count >= 1);
        let start = pins[0].pin() as usize;
        assert!(start + pins.len() <= 32);
        for i in 0..count {
            assert!(pins[i].pin() as usize == start + i, "Pins must be sequential");
        }
        self.set_out_range(start as u8, count as u8);
    }

    fn set_set_pins<'a, 'b: 'a>(&'a mut self, pins: &'b [&PioPin<Self::Pio>]) {
        let count = pins.len();
        assert!(count >= 1);
        let start = pins[0].pin() as usize;
        assert!(start + pins.len() <= 32);
        for i in 0..count {
            assert!(pins[i].pin() as usize == start + i, "Pins must be sequential");
        }
        self.set_set_range(start as u8, count as u8);
    }

    fn get_current_instr() -> u32 {
        unsafe { Self::this_sm().instr().read().0 }
    }

    fn exec_instr(&mut self, instr: u16) {
        unsafe {
            Self::this_sm().instr().write(|w| w.set_instr(instr));
        }
    }

    fn wait_push<'a>(&'a mut self, value: u32) -> FifoOutFuture<'a, Self::Pio, Self> {
        FifoOutFuture::new(self, value)
    }

    fn wait_pull<'a>(&'a mut self) -> FifoInFuture<'a, Self::Pio, Self> {
        FifoInFuture::new(self)
    }

    fn wait_irq(&self, irq_no: u8) -> IrqFuture<Self::Pio> {
        IrqFuture::new(irq_no)
    }

    fn has_tx_stalled(&self) -> bool {
        unsafe {
            let fdebug = Self::Pio::PIO.fdebug();
            let ret = fdebug.read().txstall() & (1 << Self::SM) != 0;
            fdebug.write(|w| w.set_txstall(1 << Self::SM));
            ret
        }
    }

    fn has_tx_overflowed(&self) -> bool {
        unsafe {
            let fdebug = Self::Pio::PIO.fdebug();
            let ret = fdebug.read().txover() & (1 << Self::SM) != 0;
            fdebug.write(|w| w.set_txover(1 << Self::SM));
            ret
        }
    }

    fn has_rx_stalled(&self) -> bool {
        unsafe {
            let fdebug = Self::Pio::PIO.fdebug();
            let ret = fdebug.read().rxstall() & (1 << Self::SM) != 0;
            fdebug.write(|w| w.set_rxstall(1 << Self::SM));
            ret
        }
    }

    fn has_rx_underflowed(&self) -> bool {
        unsafe {
            let fdebug = Self::Pio::PIO.fdebug();
            let ret = fdebug.read().rxunder() & (1 << Self::SM) != 0;
            fdebug.write(|w| w.set_rxunder(1 << Self::SM));
            ret
        }
    }

    fn dma_push<'a, C: Channel, W: Word>(&'a self, ch: PeripheralRef<'a, C>, data: &'a [W]) -> Transfer<'a, C> {
        unsafe {
            let pio_no = Self::Pio::PIO_NO;
            let sm_no = Self::SM;
            let p = ch.regs();
            p.read_addr().write_value(data.as_ptr() as u32);
            p.write_addr().write_value(Self::Pio::PIO.txf(sm_no).ptr() as u32);
            p.trans_count().write_value(data.len() as u32);
            compiler_fence(Ordering::SeqCst);
            p.ctrl_trig().write(|w| {
                // Set TX DREQ for this statemachine
                w.set_treq_sel(TreqSel(pio_no * 8 + sm_no as u8));
                w.set_data_size(W::size());
                w.set_chain_to(ch.number());
                w.set_incr_read(true);
                w.set_incr_write(false);
                w.set_en(true);
            });
            compiler_fence(Ordering::SeqCst);
        }
        Transfer::new(ch)
    }

    fn dma_pull<'a, C: Channel, W: Word>(&'a self, ch: PeripheralRef<'a, C>, data: &'a mut [W]) -> Transfer<'a, C> {
        unsafe {
            let pio_no = Self::Pio::PIO_NO;
            let sm_no = Self::SM;
            let p = ch.regs();
            p.write_addr().write_value(data.as_ptr() as u32);
            p.read_addr().write_value(Self::Pio::PIO.rxf(sm_no).ptr() as u32);
            p.trans_count().write_value(data.len() as u32);
            compiler_fence(Ordering::SeqCst);
            p.ctrl_trig().write(|w| {
                // Set RX DREQ for this statemachine
                w.set_treq_sel(TreqSel(pio_no * 8 + sm_no as u8 + 4));
                w.set_data_size(W::size());
                w.set_chain_to(ch.number());
                w.set_incr_read(false);
                w.set_incr_write(true);
                w.set_en(true);
            });
            compiler_fence(Ordering::SeqCst);
        }
        Transfer::new(ch)
    }
}

pub struct PioCommon<'d, PIO: PioInstance> {
    instructions_used: u32,
    pio: PhantomData<&'d PIO>,
}

pub struct PioInstanceMemory<'d, PIO: PioInstance> {
    used_mask: u32,
    pio: PhantomData<&'d PIO>,
}

impl<'d, PIO: PioInstance> PioCommon<'d, PIO> {
    pub fn write_instr<I>(&mut self, start: usize, instrs: I) -> PioInstanceMemory<'d, PIO>
    where
        I: Iterator<Item = u16>,
    {
        let mut used_mask = 0;
        for (i, instr) in instrs.enumerate() {
            let addr = (i + start) as u8;
            let mask = 1 << (addr as usize);
            assert!(
                self.instructions_used & mask == 0,
                "Trying to write already used PIO instruction memory at {}",
                addr
            );
            unsafe {
                PIO::PIO.instr_mem(addr as usize).write(|w| {
                    w.set_instr_mem(instr);
                });
            }
            used_mask |= mask;
        }
        self.instructions_used |= used_mask;
        PioInstanceMemory {
            used_mask,
            pio: PhantomData,
        }
    }

    // TODO make instruction memory that is currently in use unfreeable
    pub fn free_instr(&mut self, instrs: PioInstanceMemory<PIO>) {
        self.instructions_used &= !instrs.used_mask;
    }

    pub fn is_irq_set(&self, irq_no: u8) -> bool {
        assert!(irq_no < 8);
        unsafe {
            let irq_flags = PIO::PIO.irq();
            irq_flags.read().0 & (1 << irq_no) != 0
        }
    }

    pub fn clear_irq(&mut self, irq_no: usize) {
        assert!(irq_no < 8);
        unsafe { PIO::PIO.irq().write(|w| w.set_irq(1 << irq_no)) }
    }

    pub fn clear_irqs(&mut self, mask: u8) {
        unsafe { PIO::PIO.irq().write(|w| w.set_irq(mask)) }
    }

    pub fn force_irq(&mut self, irq_no: usize) {
        assert!(irq_no < 8);
        unsafe { PIO::PIO.irq_force().write(|w| w.set_irq_force(1 << irq_no)) }
    }

    pub fn set_input_sync_bypass<'a>(&'a mut self, bypass: u32, mask: u32) {
        unsafe {
            // this can interfere with per-pin bypass functions. splitting the
            // modification is going to be fine since nothing that relies on
            // it can reasonably run before we finish.
            PIO::PIO.input_sync_bypass().write_set(|w| *w = mask & bypass);
            PIO::PIO.input_sync_bypass().write_clear(|w| *w = mask & !bypass);
        }
    }

    pub fn get_input_sync_bypass(&self) -> u32 {
        unsafe { PIO::PIO.input_sync_bypass().read() }
    }

    pub fn make_pio_pin(&self, pin: impl Pin) -> PioPin<PIO> {
        unsafe {
            pin.io().ctrl().write(|w| w.set_funcsel(PIO::FUNCSEL.0));
        }
        PioPin {
            pin_bank: pin.pin_bank(),
            pio: PhantomData::default(),
        }
    }
}

pub struct Pio<'d, PIO: PioInstance> {
    pub common: PioCommon<'d, PIO>,
    pub sm0: PioStateMachineInstance<'d, PIO, 0>,
    pub sm1: PioStateMachineInstance<'d, PIO, 1>,
    pub sm2: PioStateMachineInstance<'d, PIO, 2>,
    pub sm3: PioStateMachineInstance<'d, PIO, 3>,
}

impl<'d, PIO: PioInstance> Pio<'d, PIO> {
    pub fn new(_pio: impl Peripheral<P = PIO> + 'd) -> Self {
        Self {
            common: PioCommon {
                instructions_used: 0,
                pio: PhantomData,
            },
            sm0: PioStateMachineInstance { pio: PhantomData },
            sm1: PioStateMachineInstance { pio: PhantomData },
            sm2: PioStateMachineInstance { pio: PhantomData },
            sm3: PioStateMachineInstance { pio: PhantomData },
        }
    }
}

pub trait PioInstance: sealed::PioInstance + Sized + Unpin {
    fn pio(&self) -> u8 {
        Self::PIO_NO
    }
}

mod sealed {
    pub trait PioStateMachine {
        type Pio: super::PioInstance;
        const SM: usize;

        #[inline(always)]
        fn this_sm() -> crate::pac::pio::StateMachine {
            Self::Pio::PIO.sm(Self::SM as usize)
        }
    }

    pub trait PioInstance {
        const PIO_NO: u8;
        const PIO: &'static crate::pac::pio::Pio;
        const FUNCSEL: crate::pac::io::vals::Gpio0ctrlFuncsel;
    }
}

macro_rules! impl_pio {
    ($name:ident, $pio:expr, $pac:ident, $funcsel:ident) => {
        impl sealed::PioInstance for peripherals::$name {
            const PIO_NO: u8 = $pio;
            const PIO: &'static pac::pio::Pio = &pac::$pac;
            const FUNCSEL: pac::io::vals::Gpio0ctrlFuncsel = pac::io::vals::Gpio0ctrlFuncsel::$funcsel;
        }
        impl PioInstance for peripherals::$name {}
    };
}

impl_pio!(PIO0, 0, PIO0, PIO0_0);
impl_pio!(PIO1, 1, PIO1, PIO1_0);
