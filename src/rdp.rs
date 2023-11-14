use crate::*;

pub struct Rdp {
}

impl Rdp {
    pub fn new() -> Rdp {
        Rdp {}
    }
}

impl Addressable for Rdp {
    fn read_u32(&mut self, offset: usize) -> Result<u32, ReadWriteFault> {
        println!("RDP: read32 offset=${:08X}", offset);
        match offset {
            // DP_STATUS 
            0x0010_000C => Ok(0),
            _ => panic!("invalid RDP read"),
        }
    }

    fn write_u32(&mut self, value: u32, offset: usize) -> Result<WriteReturnSignal, ReadWriteFault> {
        println!("RDP: write32 value=${:08X} offset=${:08X}", value, offset);
        Ok(WriteReturnSignal::None)
    }
}


