//! UART driver.

use core::cell::Cell;
use kernel::ErrorCode;

use crate::gpio;
use kernel::hil;
use kernel::utilities::cells::OptionalCell;
use kernel::utilities::cells::TakeCell;
use kernel::utilities::registers::interfaces::{ReadWriteable, Readable, Writeable};
use kernel::utilities::registers::{register_bitfields, ReadOnly, ReadWrite};
use kernel::utilities::StaticRef;

#[repr(C)]
pub struct UartRegisters {
    /// Transmit Data Register
    txdata: ReadWrite<u32, txdata::Register>,
    /// Receive Data Register
    rxdata: ReadWrite<u32, rxdata::Register>,
    /// Transmit Control Register
    txctrl: ReadWrite<u32, txctrl::Register>,
    /// Receive Control Register
    rxctrl: ReadWrite<u32, rxctrl::Register>,
    /// Interrupt Enable Register
    ie: ReadWrite<u32, interrupt::Register>,
    /// Interrupt Pending Register
    ip: ReadOnly<u32, interrupt::Register>,
    /// Baud Rate Divisor Register
    div: ReadWrite<u32, div::Register>,
}

register_bitfields![u32,
    txdata [
        full OFFSET(31) NUMBITS(1) [],
        data OFFSET(0) NUMBITS(8) []
    ],
    rxdata [
        empty OFFSET(31) NUMBITS(1) [],
        data OFFSET(0) NUMBITS(8) []
    ],
    txctrl [
        txcnt OFFSET(16) NUMBITS(3) [],
        nstop OFFSET(1) NUMBITS(1) [
            OneStopBit = 0,
            TwoStopBits = 1
        ],
        txen OFFSET(0) NUMBITS(1) []
    ],
    rxctrl [
        counter OFFSET(16) NUMBITS(3) [],
        enable OFFSET(0) NUMBITS(1) []
    ],
    interrupt [
        rxwm OFFSET(1) NUMBITS(1) [],
        txwm OFFSET(0) NUMBITS(1) []
    ],
    div [
        div OFFSET(0) NUMBITS(16) []
    ]
];

pub struct Uart<'a> {
    registers: StaticRef<UartRegisters>,
    clock_frequency: u32,
    tx_client: OptionalCell<&'a dyn hil::uart::TransmitClient>,
    rx_client: OptionalCell<&'a dyn hil::uart::ReceiveClient>,
    stop_bits: Cell<hil::uart::StopBits>,
    buffer: TakeCell<'static, [u8]>,
    len: Cell<usize>,
    index: Cell<usize>,
}

#[derive(Copy, Clone)]
pub struct UartParams {
    pub baud_rate: u32,
}

impl<'a> Uart<'a> {
    pub fn new(base: StaticRef<UartRegisters>, clock_frequency: u32) -> Uart<'a> {
        Uart {
            registers: base,
            clock_frequency: clock_frequency,
            tx_client: OptionalCell::empty(),
            rx_client: OptionalCell::empty(),
            stop_bits: Cell::new(hil::uart::StopBits::One),
            buffer: TakeCell::empty(),
            len: Cell::new(0),
            index: Cell::new(0),
        }
    }

    /// Configure GPIO pins for the UART.
    pub fn initialize_gpio_pins(&self, tx: &gpio::GpioPin, rx: &gpio::GpioPin) {
        tx.iof0();
        rx.iof0();
    }

    fn set_baud_rate(&self, baud_rate: u32) {
        let regs = self.registers;

        //            f_clk
        // f_baud = ---------
        //           div + 1
        let divisor = (self.clock_frequency / baud_rate) - 1;

        regs.div.write(div::div.val(divisor));
    }

    fn enable_tx_interrupt(&self) {
        let regs = self.registers;
        regs.ie.modify(interrupt::txwm::SET);
    }

    fn disable_tx_interrupt(&self) {
        let regs = self.registers;
        regs.ie.modify(interrupt::txwm::CLEAR);
    }

    pub fn handle_interrupt(&self) {
        let regs = self.registers;

        // Get a copy so we can check each interrupt flag in the register.
        let pending_interrupts = regs.ip.extract();

        // Determine why an interrupt occurred.
        if pending_interrupts.is_set(interrupt::txwm) {
            // Got a TX interrupt which means the number of bytes in the FIFO
            // has fallen to zero. If there is more to send do that, otherwise
            // send a callback to the client.
            if self.len.get() == self.index.get() {
                // We are done.
                regs.txctrl.write(txctrl::txen::CLEAR);
                self.disable_tx_interrupt();

                // Signal client write done
                self.tx_client.map(|client| {
                    self.buffer.take().map(|buffer| {
                        client.transmitted_buffer(buffer, self.len.get(), Ok(()));
                    });
                });
            } else {
                // More to send. Fill the buffer until it is full.
                self.buffer.map(|buffer| {
                    for i in self.index.get()..self.len.get() {
                        // Write the byte from the array to the tx register.
                        regs.txdata.write(txdata::data.val(buffer[i] as u32));
                        self.index.set(i + 1);
                        // Check if the buffer is full
                        if regs.txdata.is_set(txdata::full) {
                            // If it is, break and wait for the TX interrupt.
                            break;
                        }
                    }
                });
            }
        }
    }

    pub fn transmit_sync(&self, bytes: &[u8]) {
        let regs = self.registers;
        // Make sure the UART is enabled.
        regs.txctrl
            .write(txctrl::txen::SET + txctrl::nstop::OneStopBit + txctrl::txcnt.val(1));
        for b in bytes.iter() {
            while regs.txdata.is_set(txdata::full) {}
            regs.txdata.write(txdata::data.val(*b as u32));
        }
    }
}

impl hil::uart::Configure for Uart<'_> {
    fn configure(&self, params: hil::uart::Parameters) -> Result<(), ErrorCode> {
        // This chip does not support these features.
        if params.parity != hil::uart::Parity::None {
            return Err(ErrorCode::NOSUPPORT);
        }
        if params.hw_flow_control != false {
            return Err(ErrorCode::NOSUPPORT);
        }

        // We can set the baud rate.
        self.set_baud_rate(params.baud_rate);

        // We need to save the stop bits because it is set in the TX register.
        self.stop_bits.set(params.stop_bits);

        Ok(())
    }
}

impl<'a> hil::uart::Transmit<'a> for Uart<'a> {
    fn set_transmit_client(&self, client: &'a dyn hil::uart::TransmitClient) {
        self.tx_client.set(client);
    }

    fn transmit_buffer(
        &self,
        tx_data: &'static mut [u8],
        tx_len: usize,
    ) -> Result<(), (ErrorCode, &'static mut [u8])> {
        let regs = self.registers;

        if tx_len == 0 {
            return Err((ErrorCode::SIZE, tx_data));
        }

        // Enable the interrupt so we know when we can keep writing.
        self.enable_tx_interrupt();

        // Fill the TX buffer until it reports full.
        for i in 0..tx_len {
            // Write the byte from the array to the tx register.
            regs.txdata.write(txdata::data.val(tx_data[i] as u32));
            self.index.set(i + 1);
            // Check if the buffer is full
            if regs.txdata.is_set(txdata::full) {
                // If it is, break and wait for the TX interrupt.
                break;
            }
        }

        // Save the buffer so we can keep sending it.
        self.buffer.replace(tx_data);
        self.len.set(tx_len);

        // Enable transmissions, and wait until the FIFO is empty before getting
        // an interrupt.
        let stop_bits = match self.stop_bits.get() {
            hil::uart::StopBits::One => txctrl::nstop::OneStopBit,
            hil::uart::StopBits::Two => txctrl::nstop::TwoStopBits,
        };
        regs.txctrl
            .write(txctrl::txen::SET + stop_bits + txctrl::txcnt.val(1));

        Ok(())
    }

    fn transmit_abort(&self) -> Result<(), ErrorCode> {
        Err(ErrorCode::FAIL)
    }

    fn transmit_word(&self, _word: u32) -> Result<(), ErrorCode> {
        Err(ErrorCode::FAIL)
    }
}

impl<'a> hil::uart::Receive<'a> for Uart<'a> {
    fn set_receive_client(&self, client: &'a dyn hil::uart::ReceiveClient) {
        self.rx_client.set(client);
    }

    fn receive_buffer(
        &self,
        rx_buffer: &'static mut [u8],
        _rx_len: usize,
    ) -> Result<(), (ErrorCode, &'static mut [u8])> {
        Err((ErrorCode::FAIL, rx_buffer))
    }

    fn receive_abort(&self) -> Result<(), ErrorCode> {
        Err(ErrorCode::FAIL)
    }

    fn receive_word(&self) -> Result<(), ErrorCode> {
        Err(ErrorCode::FAIL)
    }
}
