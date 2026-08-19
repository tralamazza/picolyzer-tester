//! Hardware-looped DMA feed for the pattern state machine.
//!
//! Replaces refilling the TX FIFO from the CPU, which topped out near
//! 100 kSa/s because it had to share the main loop with USB polling. Two
//! chained channels do it without the CPU at all:
//!
//! * **data** reads pattern words into the PIO TX FIFO, paced by the state
//!   machine's DREQ so it delivers exactly as fast as samples are consumed.
//! * **reload** exists only when looping. When *data* finishes its count it
//!   chains to *reload*, which writes the buffer's start address into *data*'s
//!   `AL3_READ_ADDR_TRIG`. That single write reloads both the read address and
//!   the transfer count from its shadow register and retriggers the channel, so
//!   the loop closes in hardware with no gap and no interrupt.
//!
//! This is written against the PAC rather than rp-hal's DMA wrapper on purpose.
//! The wrapper's `double_buffer` re-arms from software, which reintroduces the
//! very CPU dependency being removed here, and its typestate would have to be
//! threaded through the state-machine ownership in `bus.rs`. Twenty lines of
//! explicit register writes are easier to check against the datasheet.

use core::sync::atomic::{AtomicU32, Ordering, compiler_fence};

use hal::pac;
use rp235x_hal as hal;

/// Channel that moves pattern words into the PIO TX FIFO.
const DATA_CH: usize = 0;
/// Channel that reloads [`DATA_CH`]'s read address to close the loop.
const RELOAD_CH: usize = 1;

/// Source word for the reload channel: the pattern buffer's start address.
///
/// A `static` because DMA reads it directly and therefore needs an address that
/// is stable for the life of the program. `AtomicU32` rather than `static mut`
/// so taking its address is not an unsafe borrow.
static RELOAD_SRC: AtomicU32 = AtomicU32::new(0);

/// The two DMA channels that drive the pattern engine.
pub struct PatternDma {
    dma: pac::DMA,
}

impl PatternDma {
    pub fn new(dma: pac::DMA, resets: &mut pac::RESETS) -> Self {
        // The DMA block comes out of chip reset held in reset, so it has to be
        // cycled before any register write sticks. rp-hal does this inside
        // `DMAExt::split` via a trait it does not export, so it is spelled out
        // here.
        resets.reset().modify(|_, w| w.dma().set_bit());
        resets.reset().modify(|_, w| w.dma().clear_bit());
        while resets.reset_done().read().dma().bit_is_clear() {}
        Self { dma }
    }

    /// Stream `words` into the PIO TX FIFO at `tx_fifo`.
    ///
    /// # Safety
    ///
    /// `words` must remain valid and unmodified until [`Self::stop`] returns.
    /// The caller guarantees this by stopping before touching the buffer.
    pub unsafe fn start(&mut self, words: &[u32], tx_fifo: *const u32, looping: bool) {
        self.stop();
        if words.is_empty() {
            return;
        }

        let base = words.as_ptr() as u32;
        RELOAD_SRC.store(base, Ordering::Relaxed);

        let data = self.dma.ch(DATA_CH);
        data.ch_read_addr().write(|w| unsafe { w.bits(base) });
        data.ch_write_addr()
            .write(|w| unsafe { w.bits(tx_fifo as u32) });
        // MODE stays NORMAL: the count is what the reload trigger restores.
        data.ch_trans_count()
            .write(|w| unsafe { w.count().bits(words.len() as u32) });

        if looping {
            let reload = self.dma.ch(RELOAD_CH);
            reload
                .ch_read_addr()
                .write(|w| unsafe { w.bits(RELOAD_SRC.as_ptr() as u32) });
            reload.ch_write_addr().write(|w| unsafe {
                w.bits(self.dma.ch(DATA_CH).ch_al3_read_addr_trig().as_ptr() as u32)
            });
            reload
                .ch_trans_count()
                .write(|w| unsafe { w.count().bits(1) });
            reload.ch_ctrl_trig().write(|w| {
                w.en().set_bit();
                w.data_size().size_word();
                // One fixed word to one fixed register: neither address moves.
                w.incr_read().clear_bit();
                w.incr_write().clear_bit();
                // No pacing: this fires once, immediately, when chained to.
                w.treq_sel().permanent();
                // Chaining to itself means "do not chain".
                unsafe { w.chain_to().bits(RELOAD_CH as u8) };
                w.irq_quiet().set_bit();
                w
            });
        }

        // The pattern was written by the CPU; make sure those stores are
        // visible before the engine is allowed to read them.
        compiler_fence(Ordering::SeqCst);

        // Writing CTRL_TRIG starts the channel, so it must be last.
        self.dma.ch(DATA_CH).ch_ctrl_trig().write(|w| {
            w.en().set_bit();
            w.data_size().size_word();
            w.incr_read().set_bit();
            // The FIFO is a single register, so the write address stays put.
            w.incr_write().clear_bit();
            // Pace on the state machine's TX request: transfers happen exactly
            // as fast as the pattern is consumed, never faster.
            w.treq_sel().pio0_tx0();
            unsafe {
                w.chain_to()
                    .bits(if looping { RELOAD_CH } else { DATA_CH } as u8)
            };
            w.irq_quiet().set_bit();
            w
        });
    }

    /// Abort both channels and leave them idle.
    pub fn stop(&mut self) {
        // Break the chain before aborting. Aborting a channel that still chains
        // to another would trigger that one on the way out, and a looping pair
        // would simply restart itself.
        for ch in [DATA_CH, RELOAD_CH] {
            self.dma
                .ch(ch)
                .ch_al1_ctrl()
                .modify(|_, w| unsafe { w.chain_to().bits(ch as u8) });
        }

        self.dma
            .chan_abort()
            .write(|w| unsafe { w.bits((1 << DATA_CH) | (1 << RELOAD_CH)) });
        // The abort bits self-clear when the channels have actually halted.
        while self.dma.chan_abort().read().bits() != 0 {}
        for ch in [DATA_CH, RELOAD_CH] {
            while self.dma.ch(ch).ch_ctrl_trig().read().busy().bit_is_set() {}
            // AL1_CTRL is the non-triggering alias, so clearing EN here does not
            // start the channel we just stopped.
            self.dma
                .ch(ch)
                .ch_al1_ctrl()
                .write(|w| unsafe { w.bits(0) });
        }

        compiler_fence(Ordering::SeqCst);
    }

    /// Whether the data channel is still transferring.
    pub fn busy(&self) -> bool {
        self.dma
            .ch(DATA_CH)
            .ch_ctrl_trig()
            .read()
            .busy()
            .bit_is_set()
    }
}
