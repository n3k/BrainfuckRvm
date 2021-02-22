
#![feature(asm)]

pub mod jitcache;
use crate::jitcache::JitCache;

use std::{collections::HashMap, io, sync::{Arc}, time::Instant};
use io::{Write, Read};
extern crate regex;
use regex::Regex;

extern crate keystone;
use keystone::{Arch, Keystone, MODE_64, OPT_SYNTAX_INTEL, OptionType};

pub const DEBUG_ENABLED: bool = false;

fn key_to_continue() {
    let mut reader = io::stdin();
    let mut buffer = [0;1];  // read exactly one byte
    reader.read_exact(&mut buffer).unwrap();
}

extern "win64" fn foo() -> u32 { 1 }

/// Reasons why the VM exited
pub enum VmExit {
    /// The VM exited due to a PTR OOB
    PtrOob,

    /// The VM exited cleanly as requested by the code.
    Exit(f64),    
}

fn rdtsc() -> u64 {
    unsafe { std::arch::x86_64::_rdtsc() }
}

pub struct Emu {
    pub memory: Vec<u8>,
    pub ptr: usize,

    jit_cache: Option<Arc<JitCache>>,
}

enum BfOperation {
    INVALID_OP,
    INC_PTR(usize),
    DEC_PTR(usize),
    INC_DATA(u8),
    DEC_DATA(u8),
    READ_STDIN,
    WRITE_STDOUT,
    LOOP_START,
    LOOP_END,
}

impl Emu {
    pub fn new(size: usize) -> Self {
        Emu {
            memory: vec![0u8; size],
            ptr: 0,
            jit_cache: None,
        }
    }

    // Enable the JIT
    pub fn enable_jit(mut self, jit_cache: Arc<JitCache>) -> Self {
        self.jit_cache = Some(jit_cache);
        self
    }

    fn receive_input(&self) -> u8 {
        let mut reader = io::stdin();
        let mut buffer = [0;1];  // read exactly one byte
        reader.read_exact(&mut buffer).unwrap();
        //return char::from(buffer[0]);
        return buffer[0];
    }

    // Run the VM using either the emulator or the JIT
    pub fn run(&mut self, instructions: String) -> Option<VmExit> {
        if let Some(jit_cache) = &self.jit_cache {
            self.run_jit(instructions)
        } else {
            self.run_vm3(instructions)
        }
    }

    pub fn run_jit(&mut self, instructions: String) -> Option<VmExit> {
        let jit_cache = self.jit_cache.as_ref().unwrap();
        println!("{:?}", self.jit_cache);

        let start = Instant::now();

        match self.generate_jit_opt(instructions) {
            Ok(machine_code) => {
                let jitted_addr = jit_cache.add_mapping(0, &machine_code);

                unsafe {
                    asm!(r#"
                       call {entry}                       
                    "#,
                    entry = in(reg) jitted_addr,
                    in("r13") self.memory.as_ptr() as usize);            
                }

            },
            Err(_) => {
                panic!("error generating machine code!")
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        Some(VmExit::Exit(elapsed))
    }

    pub fn generate_jit_opt(&self,instructions: String) -> Result<Vec<u8>, VmExit> {
        let mut idx: usize = 0;

        let mut bfInstructions = Vec::<BfOperation>::new();
        let re = Regex::new(r#"[\+]+|[-]+|[>]+|[<]+|[\[]|[\]]|[\.]|[,]"#).unwrap();
        for cap in re.captures_iter(&instructions) {
            match &cap[0].chars().nth(0).unwrap() {
                '>' => bfInstructions.push(BfOperation::INC_PTR(*&cap[0].len())),
                '<' => bfInstructions.push(BfOperation::DEC_PTR(*&cap[0].len())),
                '+' => bfInstructions.push(BfOperation::INC_DATA(*&cap[0].len() as u8)),
                '-' => bfInstructions.push(BfOperation::DEC_DATA(*&cap[0].len() as u8)),
                '.' => bfInstructions.push(BfOperation::WRITE_STDOUT),
                ',' => bfInstructions.push(BfOperation::READ_STDIN),
                '[' => bfInstructions.push(BfOperation::LOOP_START),
                ']' => bfInstructions.push(BfOperation::LOOP_END),
                _ => {
                    unreachable!("xxx");
                }
            }            
        }

        let mut asm = String::new();

        let engine = Keystone::new(Arch::X86, keystone::MODE_64)
            .expect("Could not initialize keystone engine");
        engine.option(OptionType::SYNTAX, keystone::OPT_SYNTAX_INTEL)
            .expect("Could not set option to intel syntax");

        idx = 0;       

        let mut labels: u64 = 0;
        let mut forward_labels = Vec::<u64>::new();
        let mut backward_labels = Vec::<u64>::new();
            
        while idx < bfInstructions.len() {
            let operation = bfInstructions.get(idx).unwrap();
            // Decode operator
            match operation {
                BfOperation::INC_PTR(times) => {
                    // Increment the data pointer to the next cell
                    asm += &format!(r#"
                        add r13, 0x{:x};
                    "#, times);                   
                },
                BfOperation::DEC_PTR(times) => {
                    // Decrement the data pointer to point to the previous cell      
                    asm += &format!(r#"
                        sub r13, 0x{:x};
                    "#, times);                  
                },
                BfOperation::INC_DATA(times) => {
                    asm += &format!(r#"
                        add qword ptr ds:[r13], 0x{:x};
                    "#, times);              
                },
                BfOperation::DEC_DATA(times) => {
                    // Decrement the byte value at data pointer.
                    asm += &format!(r#"
                        sub qword ptr ds:[r13], 0x{:x};
                    "#, times);  
                },
                BfOperation::WRITE_STDOUT => {
                    // Output the byte value at the data pointer.
                    asm += &format!(r#"
                        mov rax, 1;
                        mov rdi, 1;
                        mov rsi, r13;
                        mov rdx, 1;                       
                        syscall;
                    "#);                         
                },
                BfOperation::READ_STDIN => {
                    // Input one byte and store its value at the data pointer.
                    asm += &format!(r#"
                        mov rax, 0;
                        mov rdi, 0;
                        mov rsi, r13;
                        mov rdx, 1;
                        syscall;
                    "#);                
                },
                BfOperation::LOOP_START => {
                    asm += &format!(r#"
                        label{}:
                        "#, labels);

                    backward_labels.push(labels);
                    labels += 1;

                    asm += &format!(r#"
                        cmp byte ptr ds:[r13], 0;                        
                    "#);
                    
                    asm += &format!(r#"
                        jz label{};                        
                    "#, labels);
                    forward_labels.push(labels);
                    labels += 1;
                },
                BfOperation::LOOP_END => {
                    // Unconditionally jump back to the matching [ bracket.
                    asm += &format!(r#"
                        jmp label{};
                        label{}:
                    "#, backward_labels.pop().unwrap(),
                        forward_labels.pop().unwrap()
                    ); 
                },
                _ => { panic!("unrecognized token at position {}", idx) }
            }

            idx += 1;
        }   
        
        asm += &format!(r#"            
            ret;
        "#);
   
        let result = engine.asm(asm.to_string(), 0)
        .expect(&format!("could not assemble:\n{}", asm)); 

        Ok(result.bytes)
    }


    /// JIT The stuff up
    pub fn generate_jit(&self,instructions: String) -> Result<Vec<u8>, VmExit> {
        let mut asm = String::new();

        let engine = Keystone::new(Arch::X86, keystone::MODE_64)
            .expect("Could not initialize keystone engine");
        engine.option(OptionType::SYNTAX, keystone::OPT_SYNTAX_INTEL)
            .expect("Could not set option to intel syntax");

        let mut idx: usize = 0;

        let mut labels: u64 = 0;
        let mut forward_labels = Vec::<u64>::new();
        let mut backward_labels = Vec::<u64>::new();
        let instructions = instructions.as_bytes();
        while idx < instructions.len() {
            let operation = instructions.get(idx).unwrap();
            // Decode operator
            match operation {
                b'>' => {                    
                    // inc %r13
                    asm += &format!(r#"
                        inc r13;
                    "#);
                    // 0x49, 0xFF, 0xC5  
                },
                b'<' => {                    
                    asm += &format!(r#"
                        dec r13;
                    "#);                    
                },
                b'+' => {                    
                    // addb $1, 0(%r13)
                    asm += &format!(r#"
                        add qword ptr ds:[r13], 1;
                    "#);      
                },
                b'-' => {
                    // Decrement the byte value at data pointer.
                    asm += &format!(r#"
                        sub qword ptr ds:[r13], 1;
                    "#);                    
                },
                b'.' => {
                    // Output the byte value at the data pointer.
                   
                    asm += &format!(r#"
                        mov rax, 1;
                        mov rdi, 1;
                        mov rsi, r13;
                        mov rdx, 1;
                        syscall;
                    "#);                     
                },
                b',' => {
                    asm += &format!(r#"
                        mov rax, 0;
                        mov rdi, 0;
                        mov rsi, r13;
                        mov rdx, 1;
                        syscall;
                    "#);      
                },
                b'[' => {
                    asm += &format!(r#"
                        label{}:
                        "#, labels);

                    backward_labels.push(labels);
                    labels += 1;

                    asm += &format!(r#"
                        cmp byte ptr ds:[r13], 0;                        
                    "#);
                    
                    asm += &format!(r#"
                        jz label{};                        
                    "#, labels);
                    forward_labels.push(labels);
                    labels += 1;

                },
                b']' => {
                    // Unconditionally jump back to the matching [ bracket.
                    asm += &format!(r#"
                        jmp label{};
                        label{}:
                    "#, backward_labels.pop().unwrap(),
                        forward_labels.pop().unwrap()
                    );                    
                  
                },
                _ => { panic!("unrecognized token at position {}", idx) }
            }

                idx += 1;
            }

            asm += &format!(r#"
               ret;
            "#);
          
            let result = engine.asm(asm.to_string(), 0)
            .expect(&format!("could not assemble:\n{}", asm));            

            Ok(result.bytes)
    }
    

    pub fn run_vm(&mut self, instructions: String) -> Option<VmExit> {
        // flag to indicate that we need to scan for the matching `]`
        let mut scan_loop_end = false;
        /// loop nesting consideration
        let mut nested_depth = 0u32;

        /// used to keep track of [] loops
        //let mut loop_positions = Vec::<usize>::new();
        let mut start_loop_positions = Vec::<usize>::new();
        let mut end_loop_positions = Vec::<usize>::new();

        let mut idx: usize = 0;

        // start a timer
        let start = Instant::now();
            
        let instructions = instructions.as_bytes();
        while idx < instructions.len() {
            let operation = instructions.get(idx).unwrap();
            // Decode operator
            match operation {
                b'>' => {
                    // Increment the data pointer to the next cell
                    if scan_loop_end == false {
                        if (self.ptr + 1) >= self.memory.len() {
                            return Some(VmExit::PtrOob);
                        }
                        self.ptr += 1;
                        if DEBUG_ENABLED {
                            println!("Executed Op: > at pos {} - ptr: {}", idx, self.ptr);                             
                        }                    
                    }
                },
                b'<' => {
                    // Decrement the data pointer to point to the previous cell       
                    if scan_loop_end == false {
                        if self.ptr == 0 {
                            return Some(VmExit::PtrOob);
                        } 
                        self.ptr -= 1;
                        if DEBUG_ENABLED {
                            println!("Executed Op: < at pos {} - ptr: {}", idx, self.ptr);                     
                        }
                    }

                },
                b'+' => {
                    // Increment the byte value at data pointer
                    if scan_loop_end == false {
                        self.memory[self.ptr] += 1;
                        if DEBUG_ENABLED {
                            println!("Executed Op: + at pos {} - ptr: {}", idx, self.ptr); 
                        
                        }   
                    }             
                },
                b'-' => {
                    // Decrement the byte value at data pointer.
                    if scan_loop_end == false {
                        self.memory[self.ptr] -= 1;
                        if DEBUG_ENABLED {
                            println!("Executed Op: - at pos {} - ptr: {}", idx, self.ptr);   
                        }
                    }
                },
                b'.' => {
                    // Output the byte value at the data pointer.
                   
                    if scan_loop_end == false {
                        print!("{}", char::from(self.memory[self.ptr]));
                        io::stdout().flush().ok().expect("Could not flush stdout");
                        if DEBUG_ENABLED {
                            println!("Executed Op: . at pos {} - ptr: {}", idx, self.ptr); 
                        }
                    }                          
                },
                b',' => {
                    // Input one byte and store its value at the data pointer.
                    if scan_loop_end == false {
                        self.memory[self.ptr] = self.receive_input();

                        if DEBUG_ENABLED {
                            println!("Executed Op: , at pos {} - ptr: {}", idx, self.ptr);  
                        }
                    }
                },
                b'[' => {
                    // If the byte value at the data pointer is zero,
                    // jump to the instruction following the matching ] bracket.
                    // Otherwise, continue execution.            

                    if self.memory[self.ptr] == 0  {                        
                        /*if let Some(pos) = end_loop_positions.pop() {                          
                            idx = pos + 1;
                            if idx >= instructions.len() {
                                break;
                            }
                            continue;
                        }
                        else {
                        */
                        scan_loop_end = true;

                        //}
                    }

                    if scan_loop_end == false {
                        end_loop_positions.pop();
                        start_loop_positions.push(idx);
                    } else {
                        nested_depth += 1;
                    }

                },
                b']' => {
                    // Unconditionally jump back to the matching [ bracket.
                
                    
                    if scan_loop_end == true {
                        nested_depth -= 1;
                        if nested_depth == 0 {
                            scan_loop_end = false;
                        }
                    } else {       
                        //end_loop_positions.push(idx);                 
                        idx = start_loop_positions.pop().unwrap();                        
                        continue;
                    }
                  
                },
                _ => { panic!("unrecognized token at position {}", idx) }
            }

            idx += 1;
        }        

        let elapsed = start.elapsed().as_secs_f64();
        Some(VmExit::Exit(elapsed))
    }


    /// Same as before but it precomputes the loops `[` `]`
    pub fn run_vm2(&mut self, instructions: String) -> Option<VmExit> {
    
        let mut idx: usize = 0;

        //let mut loop_map = HashMap::<usize, usize>::new();
        let mut loop_map = vec![0usize; instructions.len()];

        let instructions = instructions.as_bytes();
        while idx < instructions.len() {
            let operation = instructions.get(idx).unwrap();
            if *operation == b'[' {
                let mut bracket_nesting = 1u32;
                let mut seek = idx + 1;                
                while seek < instructions.len() { 
                    let cur_ins = *instructions.get(seek).unwrap();                   
                    if cur_ins == b']' {
                        bracket_nesting -= 1;
                    } else if cur_ins == b'[' {
                        bracket_nesting += 1;
                    }

                    if bracket_nesting == 0 {
                        break;
                    }
                    seek += 1;
                }

                if bracket_nesting == 0 {
                    //loop_map.insert(idx, seek);
                    //loop_map.insert(seek, idx);
                    loop_map[idx] = seek;
                    loop_map[seek] = idx;
                } else {
                    panic!("unmatched `[` at pos: {}", idx);
                }
            }

            idx += 1;
        }

        idx = 0;
        // start a timer
        let start = Instant::now();
            
        while idx < instructions.len() {
            let operation = instructions.get(idx).unwrap();
            // Decode operator
            match operation {
                b'>' => {
                    // Increment the data pointer to the next cell
                    if (self.ptr + 1) >= self.memory.len() {
                        return Some(VmExit::PtrOob);
                    }
                    self.ptr += 1;
                    if DEBUG_ENABLED {
                        println!("Executed Op: > at pos {} - ptr: {}", idx, self.ptr);                             
                    }                    
                },
                b'<' => {
                    // Decrement the data pointer to point to the previous cell      
                    if self.ptr == 0 {
                        return Some(VmExit::PtrOob);
                    } 
                    self.ptr -= 1;
                    if DEBUG_ENABLED {
                        println!("Executed Op: < at pos {} - ptr: {}", idx, self.ptr);                     
                    }                  
                },
                b'+' => {
                    // Increment the byte value at data pointer                    
                    self.memory[self.ptr] = self.memory[self.ptr].wrapping_add(1);
                    if DEBUG_ENABLED {
                        println!("Executed Op: + at pos {} - ptr: {}", idx, self.ptr); 
                    
                    }               
                },
                b'-' => {
                    // Decrement the byte value at data pointer.
                    self.memory[self.ptr] = self.memory[self.ptr].wrapping_sub(1);
                    if DEBUG_ENABLED {
                        println!("Executed Op: - at pos {} - ptr: {}", idx, self.ptr);   
                    }
                },
                b'.' => {
                    // Output the byte value at the data pointer.
                    print!("{}", char::from(self.memory[self.ptr]));
                    io::stdout().flush().ok().expect("Could not flush stdout");
                    if DEBUG_ENABLED {
                        println!("Executed Op: . at pos {} - ptr: {}", idx, self.ptr); 
                    }                        
                },
                b',' => {
                    // Input one byte and store its value at the data pointer.
                    self.memory[self.ptr] = self.receive_input();

                    if DEBUG_ENABLED {
                        println!("Executed Op: , at pos {} - ptr: {}", idx, self.ptr);  
                    }                
                },
                b'[' => {
                    // If the byte value at the data pointer is zero,
                    // jump to the instruction following the matching ] bracket.
                    // Otherwise, continue execution.          
                    if self.memory[self.ptr] == 0  {                        
                        //idx = *loop_map.get(&idx).unwrap();
                        idx = loop_map[idx];
                        continue;
                    }
                },
                b']' => {
                    // Unconditionally jump back to the matching [ bracket.
                    if self.memory[self.ptr] != 0  {                        
                        //idx = *loop_map.get(&idx).unwrap();
                        idx = loop_map[idx];
                        continue;
                    }
                },
                _ => { panic!("unrecognized token at position {}", idx) }
            }

            idx += 1;
        }        

        let elapsed = start.elapsed().as_secs_f64();
        Some(VmExit::Exit(elapsed))
    }


    /// Same as run_vm2 but it consolidates sequences of operations
    pub fn run_vm3(&mut self, instructions: String) -> Option<VmExit> {
    
        let mut idx: usize = 0;

        let mut bfInstructions = Vec::<BfOperation>::new();
        let re = Regex::new(r#"[\+]+|[-]+|[>]+|[<]+|[\[]|[\]]|[\.]|[,]"#).unwrap();
        for cap in re.captures_iter(&instructions) {
            match &cap[0].chars().nth(0).unwrap() {
                '>' => bfInstructions.push(BfOperation::INC_PTR(*&cap[0].len())),
                '<' => bfInstructions.push(BfOperation::DEC_PTR(*&cap[0].len())),
                '+' => bfInstructions.push(BfOperation::INC_DATA(*&cap[0].len() as u8)),
                '-' => bfInstructions.push(BfOperation::DEC_DATA(*&cap[0].len() as u8)),
                '.' => bfInstructions.push(BfOperation::WRITE_STDOUT),
                ',' => bfInstructions.push(BfOperation::READ_STDIN),
                '[' => bfInstructions.push(BfOperation::LOOP_START),
                ']' => bfInstructions.push(BfOperation::LOOP_END),
                _ => {
                    unreachable!("xxx");
                }
            }            
        }

        // Precompute loops
        let mut loop_map = vec![0usize; bfInstructions.len()];
        idx = 0;
        while idx < bfInstructions.len() {
            let operation = bfInstructions.get(idx).unwrap();
            match operation {
                BfOperation::LOOP_START => {
                    let mut bracket_nesting = 1u32;
                    let mut seek = idx + 1;                
                    while seek < instructions.len() { 
                        let cur_ins = bfInstructions.get(seek).unwrap();
                        match cur_ins {
                            BfOperation::LOOP_START => {
                                bracket_nesting += 1;
                            },
                            BfOperation::LOOP_END => {
                                bracket_nesting -= 1;
                            }
                            _ => {}
                        }  

                        if bracket_nesting == 0 {
                            break;
                        }
                        seek += 1;
                    }

                    if bracket_nesting == 0 {
                        //loop_map.insert(idx, seek);
                        //loop_map.insert(seek, idx);
                        loop_map[idx] = seek;
                        loop_map[seek] = idx;
                    } else {
                        panic!("unmatched `[` at pos: {}", idx);
                    }
                }
                _ => {}
            }

            idx += 1;
        }

        

        idx = 0;
        // start a timer
        let start = Instant::now();
            
        while idx < bfInstructions.len() {
            let operation = bfInstructions.get(idx).unwrap();
            // Decode operator
            match operation {
                BfOperation::INC_PTR(times) => {
                    // Increment the data pointer to the next cell
                    if (self.ptr + times) >= self.memory.len() {
                        return Some(VmExit::PtrOob);
                    }
                    self.ptr += times;
                    if DEBUG_ENABLED {
                        println!("Executed Op: > at pos {} - ptr: {}", idx, self.ptr);                             
                    }                    
                },
                BfOperation::DEC_PTR(times) => {
                    // Decrement the data pointer to point to the previous cell      
                    if self.ptr == 0 {
                        return Some(VmExit::PtrOob);
                    } 
                    self.ptr -= times;
                    if DEBUG_ENABLED {
                        println!("Executed Op: < at pos {} - ptr: {}", idx, self.ptr);                     
                    }                  
                },
                BfOperation::INC_DATA(times) => {
                    // Increment the byte value at data pointer                    
                    self.memory[self.ptr] = self.memory[self.ptr].wrapping_add(*times);
                    if DEBUG_ENABLED {
                        println!("Executed Op: + at pos {} - ptr: {}", idx, self.ptr); 
                    
                    }               
                },
                BfOperation::DEC_DATA(times) => {
                    // Decrement the byte value at data pointer.
                    self.memory[self.ptr] = self.memory[self.ptr].wrapping_sub(*times);
                    if DEBUG_ENABLED {
                        println!("Executed Op: - at pos {} - ptr: {}", idx, self.ptr);   
                    }
                },
                BfOperation::WRITE_STDOUT => {
                    // Output the byte value at the data pointer.
                    print!("{}", char::from(self.memory[self.ptr]));
                    io::stdout().flush().ok().expect("Could not flush stdout");
                    if DEBUG_ENABLED {
                        println!("Executed Op: . at pos {} - ptr: {}", idx, self.ptr); 
                    }                        
                },
                BfOperation::READ_STDIN => {
                    // Input one byte and store its value at the data pointer.
                    self.memory[self.ptr] = self.receive_input();

                    if DEBUG_ENABLED {
                        println!("Executed Op: , at pos {} - ptr: {}", idx, self.ptr);  
                    }                
                },
                BfOperation::LOOP_START => {
                    // If the byte value at the data pointer is zero,
                    // jump to the instruction following the matching ] bracket.
                    // Otherwise, continue execution.          
                    if self.memory[self.ptr] == 0  {                        
                        //idx = *loop_map.get(&idx).unwrap();
                        idx = loop_map[idx];
                        continue;
                    }
                },
                BfOperation::LOOP_END => {
                    // Unconditionally jump back to the matching [ bracket.
                    if self.memory[self.ptr] != 0  {                        
                        //idx = *loop_map.get(&idx).unwrap();
                        idx = loop_map[idx];
                        continue;
                    }
                },
                _ => { panic!("unrecognized token at position {}", idx) }
            }

            idx += 1;
        }        

        let elapsed = start.elapsed().as_secs_f64();
        Some(VmExit::Exit(elapsed))
    }
}


fn remove_whitespace(s: &mut String) {
    s.retain(|c| !c.is_whitespace());
}

fn plot_mandelbrot() {
    let mut bfcode = r#"
    +++++++++++++[->++>>>+++++>++>+<<<<<<]>>>>>++++++>--->>>>>>>>>>+++++++++++++++[[
>>>>>>>>>]+[<<<<<<<<<]>>>>>>>>>-]+[>>>>>>>>[-]>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>[-]+
<<<<<<<+++++[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>>>>+>>>>>>>>>>>>>>>>>>>>>>>>>>
>+<<<<<<<<<<<<<<<<<[<<<<<<<<<]>>>[-]+[>>>>>>[>>>>>>>[-]>>]<<<<<<<<<[<<<<<<<<<]>>
>>>>>[-]+<<<<<<++++[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>>>+<<<<<<+++++++[-[->>>
>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>>>+<<<<<<<<<<<<<<<<[<<<<<<<<<]>>>[[-]>>>>>>[>>>>>
>>[-<<<<<<+>>>>>>]<<<<<<[->>>>>>+<<+<<<+<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>
[>>>>>>>>[-<<<<<<<+>>>>>>>]<<<<<<<[->>>>>>>+<<+<<<+<<]>>>>>>>>]<<<<<<<<<[<<<<<<<
<<]>>>>>>>[-<<<<<<<+>>>>>>>]<<<<<<<[->>>>>>>+<<+<<<<<]>>>>>>>>>+++++++++++++++[[
>>>>>>>>>]+>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+[
>+>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>[-<<<<+>>>>]<<<<[->>>>+<<<<<[->>[
-<<+>>]<<[->>+>>+<<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>>>>>>>>]<<<<<<<
<<[>[->>>>>>>>>+<<<<<<<<<]<<<<<<<<<<]>[->>>>>>>>>+<<<<<<<<<]<+>>>>>>>>]<<<<<<<<<
[>[-]<->>>>[-<<<<+>[<->-<<<<<<+>>>>>>]<[->+<]>>>>]<<<[->>>+<<<]<+<<<<<<<<<]>>>>>
>>>>[>+>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>>[-<<<<<+>>>>>]<<<<<[->>>>>+
<<<<<<[->>>[-<<<+>>>]<<<[->>>+>+<<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>
>>>>>>>]<<<<<<<<<[>>[->>>>>>>>>+<<<<<<<<<]<<<<<<<<<<<]>>[->>>>>>>>>+<<<<<<<<<]<<
+>>>>>>>>]<<<<<<<<<[>[-]<->>>>[-<<<<+>[<->-<<<<<<+>>>>>>]<[->+<]>>>>]<<<[->>>+<<
<]<+<<<<<<<<<]>>>>>>>>>[>>>>[-<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<+>>>>>>>>>>>>>
>>>>>>>>>>>>>>>>>>>>>>>]>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>+++++++++++++++[[>>>>
>>>>>]<<<<<<<<<-<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+>>>>>>>>>>>>>>>>>>>>>+<<<[<<<<<<
<<<]>>>>>>>>>[>>>[-<<<->>>]+<<<[->>>->[-<<<<+>>>>]<<<<[->>>>+<<<<<<<<<<<<<[<<<<<
<<<<]>>>>[-]+>>>>>[>>>>>>>>>]>+<]]+>>>>[-<<<<->>>>]+<<<<[->>>>-<[-<<<+>>>]<<<[->
>>+<<<<<<<<<<<<[<<<<<<<<<]>>>[-]+>>>>>>[>>>>>>>>>]>[-]+<]]+>[-<[>>>>>>>>>]<<<<<<
<<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]<<<<<<<[->+>>>-<<<<]>>>>>>>>>+++++++++++++++++++
+++++++>>[-<<<<+>>>>]<<<<[->>>>+<<[-]<<]>>[<<<<<<<+<[-<+>>>>+<<[-]]>[-<<[->+>>>-
<<<<]>>>]>>>>>>>>>>>>>[>>[-]>[-]>[-]>>>>>]<<<<<<<<<[<<<<<<<<<]>>>[-]>>>>>>[>>>>>
[-<<<<+>>>>]<<<<[->>>>+<<<+<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>>[-<<<<<<<<
<+>>>>>>>>>]>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>+++++++++++++++[[>>>>>>>>>]+>[-
]>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+[>+>>>>>>>>]<<<
<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>>[-<<<<<+>>>>>]<<<<<[->>>>>+<<<<<<[->>[-<<+>>]<
<[->>+>+<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>>>>>>>>]<<<<<<<<<[>[->>>>
>>>>>+<<<<<<<<<]<<<<<<<<<<]>[->>>>>>>>>+<<<<<<<<<]<+>>>>>>>>]<<<<<<<<<[>[-]<->>>
[-<<<+>[<->-<<<<<<<+>>>>>>>]<[->+<]>>>]<<[->>+<<]<+<<<<<<<<<]>>>>>>>>>[>>>>>>[-<
<<<<+>>>>>]<<<<<[->>>>>+<<<<+<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>+>>>>>>>>
]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>>[-<<<<<+>>>>>]<<<<<[->>>>>+<<<<<<[->>[-<<+
>>]<<[->>+>>+<<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>>>>>>>>]<<<<<<<<<[>
[->>>>>>>>>+<<<<<<<<<]<<<<<<<<<<]>[->>>>>>>>>+<<<<<<<<<]<+>>>>>>>>]<<<<<<<<<[>[-
]<->>>>[-<<<<+>[<->-<<<<<<+>>>>>>]<[->+<]>>>>]<<<[->>>+<<<]<+<<<<<<<<<]>>>>>>>>>
[>>>>[-<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<+>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
]>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>>>[-<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<+>
>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>]>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>++++++++
+++++++[[>>>>>>>>>]<<<<<<<<<-<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+[>>>>>>>>[-<<<<<<<+
>>>>>>>]<<<<<<<[->>>>>>>+<<<<<<+<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>>>>>>[
-]>>>]<<<<<<<<<[<<<<<<<<<]>>>>+>[-<-<<<<+>>>>>]>[-<<<<<<[->>>>>+<++<<<<]>>>>>[-<
<<<<+>>>>>]<->+>]<[->+<]<<<<<[->>>>>+<<<<<]>>>>>>[-]<<<<<<+>>>>[-<<<<->>>>]+<<<<
[->>>>->>>>>[>>[-<<->>]+<<[->>->[-<<<+>>>]<<<[->>>+<<<<<<<<<<<<[<<<<<<<<<]>>>[-]
+>>>>>>[>>>>>>>>>]>+<]]+>>>[-<<<->>>]+<<<[->>>-<[-<<+>>]<<[->>+<<<<<<<<<<<[<<<<<
<<<<]>>>>[-]+>>>>>[>>>>>>>>>]>[-]+<]]+>[-<[>>>>>>>>>]<<<<<<<<]>>>>>>>>]<<<<<<<<<
[<<<<<<<<<]>>>>[-<<<<+>>>>]<<<<[->>>>+>>>>>[>+>>[-<<->>]<<[->>+<<]>>>>>>>>]<<<<<
<<<+<[>[->>>>>+<<<<[->>>>-<<<<<<<<<<<<<<+>>>>>>>>>>>[->>>+<<<]<]>[->>>-<<<<<<<<<
<<<<<+>>>>>>>>>>>]<<]>[->>>>+<<<[->>>-<<<<<<<<<<<<<<+>>>>>>>>>>>]<]>[->>>+<<<]<<
<<<<<<<<<<]>>>>[-]<<<<]>>>[-<<<+>>>]<<<[->>>+>>>>>>[>+>[-<->]<[->+<]>>>>>>>>]<<<
<<<<<+<[>[->>>>>+<<<[->>>-<<<<<<<<<<<<<<+>>>>>>>>>>[->>>>+<<<<]>]<[->>>>-<<<<<<<
<<<<<<<+>>>>>>>>>>]<]>>[->>>+<<<<[->>>>-<<<<<<<<<<<<<<+>>>>>>>>>>]>]<[->>>>+<<<<
]<<<<<<<<<<<]>>>>>>+<<<<<<]]>>>>[-<<<<+>>>>]<<<<[->>>>+>>>>>[>>>>>>>>>]<<<<<<<<<
[>[->>>>>+<<<<[->>>>-<<<<<<<<<<<<<<+>>>>>>>>>>>[->>>+<<<]<]>[->>>-<<<<<<<<<<<<<<
+>>>>>>>>>>>]<<]>[->>>>+<<<[->>>-<<<<<<<<<<<<<<+>>>>>>>>>>>]<]>[->>>+<<<]<<<<<<<
<<<<<]]>[-]>>[-]>[-]>>>>>[>>[-]>[-]>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>>>>>[-<
<<<+>>>>]<<<<[->>>>+<<<+<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>+++++++++++++++[
[>>>>>>>>>]+>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+
[>+>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>[-<<<<+>>>>]<<<<[->>>>+<<<<<[->>
[-<<+>>]<<[->>+>+<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>>>>>>>>]<<<<<<<<
<[>[->>>>>>>>>+<<<<<<<<<]<<<<<<<<<<]>[->>>>>>>>>+<<<<<<<<<]<+>>>>>>>>]<<<<<<<<<[
>[-]<->>>[-<<<+>[<->-<<<<<<<+>>>>>>>]<[->+<]>>>]<<[->>+<<]<+<<<<<<<<<]>>>>>>>>>[
>>>[-<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<+>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>]>
>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>[-]>>>>+++++++++++++++[[>>>>>>>>>]<<<<<<<<<-<<<<<
<<<<[<<<<<<<<<]>>>>>>>>>-]+[>>>[-<<<->>>]+<<<[->>>->[-<<<<+>>>>]<<<<[->>>>+<<<<<
<<<<<<<<[<<<<<<<<<]>>>>[-]+>>>>>[>>>>>>>>>]>+<]]+>>>>[-<<<<->>>>]+<<<<[->>>>-<[-
<<<+>>>]<<<[->>>+<<<<<<<<<<<<[<<<<<<<<<]>>>[-]+>>>>>>[>>>>>>>>>]>[-]+<]]+>[-<[>>
>>>>>>>]<<<<<<<<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>[-<<<+>>>]<<<[->>>+>>>>>>[>+>>>
[-<<<->>>]<<<[->>>+<<<]>>>>>>>>]<<<<<<<<+<[>[->+>[-<-<<<<<<<<<<+>>>>>>>>>>>>[-<<
+>>]<]>[-<<-<<<<<<<<<<+>>>>>>>>>>>>]<<<]>>[-<+>>[-<<-<<<<<<<<<<+>>>>>>>>>>>>]<]>
[-<<+>>]<<<<<<<<<<<<<]]>>>>[-<<<<+>>>>]<<<<[->>>>+>>>>>[>+>>[-<<->>]<<[->>+<<]>>
>>>>>>]<<<<<<<<+<[>[->+>>[-<<-<<<<<<<<<<+>>>>>>>>>>>[-<+>]>]<[-<-<<<<<<<<<<+>>>>
>>>>>>>]<<]>>>[-<<+>[-<-<<<<<<<<<<+>>>>>>>>>>>]>]<[-<+>]<<<<<<<<<<<<]>>>>>+<<<<<
]>>>>>>>>>[>>>[-]>[-]>[-]>>>>]<<<<<<<<<[<<<<<<<<<]>>>[-]>[-]>>>>>[>>>>>>>[-<<<<<
<+>>>>>>]<<<<<<[->>>>>>+<<<<+<<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>+>[-<-<<<<+>>>>
>]>>[-<<<<<<<[->>>>>+<++<<<<]>>>>>[-<<<<<+>>>>>]<->+>>]<<[->>+<<]<<<<<[->>>>>+<<
<<<]+>>>>[-<<<<->>>>]+<<<<[->>>>->>>>>[>>>[-<<<->>>]+<<<[->>>-<[-<<+>>]<<[->>+<<
<<<<<<<<<[<<<<<<<<<]>>>>[-]+>>>>>[>>>>>>>>>]>+<]]+>>[-<<->>]+<<[->>->[-<<<+>>>]<
<<[->>>+<<<<<<<<<<<<[<<<<<<<<<]>>>[-]+>>>>>>[>>>>>>>>>]>[-]+<]]+>[-<[>>>>>>>>>]<
<<<<<<<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>[-<<<+>>>]<<<[->>>+>>>>>>[>+>[-<->]<[->+
<]>>>>>>>>]<<<<<<<<+<[>[->>>>+<<[->>-<<<<<<<<<<<<<+>>>>>>>>>>[->>>+<<<]>]<[->>>-
<<<<<<<<<<<<<+>>>>>>>>>>]<]>>[->>+<<<[->>>-<<<<<<<<<<<<<+>>>>>>>>>>]>]<[->>>+<<<
]<<<<<<<<<<<]>>>>>[-]>>[-<<<<<<<+>>>>>>>]<<<<<<<[->>>>>>>+<<+<<<<<]]>>>>[-<<<<+>
>>>]<<<<[->>>>+>>>>>[>+>>[-<<->>]<<[->>+<<]>>>>>>>>]<<<<<<<<+<[>[->>>>+<<<[->>>-
<<<<<<<<<<<<<+>>>>>>>>>>>[->>+<<]<]>[->>-<<<<<<<<<<<<<+>>>>>>>>>>>]<<]>[->>>+<<[
->>-<<<<<<<<<<<<<+>>>>>>>>>>>]<]>[->>+<<]<<<<<<<<<<<<]]>>>>[-]<<<<]>>>>[-<<<<+>>
>>]<<<<[->>>>+>[-]>>[-<<<<<<<+>>>>>>>]<<<<<<<[->>>>>>>+<<+<<<<<]>>>>>>>>>[>>>>>>
>>>]<<<<<<<<<[>[->>>>+<<<[->>>-<<<<<<<<<<<<<+>>>>>>>>>>>[->>+<<]<]>[->>-<<<<<<<<
<<<<<+>>>>>>>>>>>]<<]>[->>>+<<[->>-<<<<<<<<<<<<<+>>>>>>>>>>>]<]>[->>+<<]<<<<<<<<
<<<<]]>>>>>>>>>[>>[-]>[-]>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>[-]>[-]>>>>>[>>>>>[-<<<<+
>>>>]<<<<[->>>>+<<<+<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>>>>>>[-<<<<<+>>>>>
]<<<<<[->>>>>+<<<+<<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>+++++++++++++++[[>>>>
>>>>>]+>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]>[-]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+[>+>>
>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>[-<<<<+>>>>]<<<<[->>>>+<<<<<[->>[-<<+
>>]<<[->>+>>+<<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>>>>>>>>]<<<<<<<<<[>
[->>>>>>>>>+<<<<<<<<<]<<<<<<<<<<]>[->>>>>>>>>+<<<<<<<<<]<+>>>>>>>>]<<<<<<<<<[>[-
]<->>>>[-<<<<+>[<->-<<<<<<+>>>>>>]<[->+<]>>>>]<<<[->>>+<<<]<+<<<<<<<<<]>>>>>>>>>
[>+>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>->>>>>[-<<<<<+>>>>>]<<<<<[->>>>>+<<<<
<<[->>>[-<<<+>>>]<<<[->>>+>+<<<<]+>>>>>>>>>]<<<<<<<<[<<<<<<<<<]]>>>>>>>>>[>>>>>>
>>>]<<<<<<<<<[>>[->>>>>>>>>+<<<<<<<<<]<<<<<<<<<<<]>>[->>>>>>>>>+<<<<<<<<<]<<+>>>
>>>>>]<<<<<<<<<[>[-]<->>>>[-<<<<+>[<->-<<<<<<+>>>>>>]<[->+<]>>>>]<<<[->>>+<<<]<+
<<<<<<<<<]>>>>>>>>>[>>>>[-<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<+>>>>>>>>>>>>>>>>>
>>>>>>>>>>>>>>>>>>>]>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>+++++++++++++++[[>>>>>>>>
>]<<<<<<<<<-<<<<<<<<<[<<<<<<<<<]>>>>>>>>>-]+>>>>>>>>>>>>>>>>>>>>>+<<<[<<<<<<<<<]
>>>>>>>>>[>>>[-<<<->>>]+<<<[->>>->[-<<<<+>>>>]<<<<[->>>>+<<<<<<<<<<<<<[<<<<<<<<<
]>>>>[-]+>>>>>[>>>>>>>>>]>+<]]+>>>>[-<<<<->>>>]+<<<<[->>>>-<[-<<<+>>>]<<<[->>>+<
<<<<<<<<<<<[<<<<<<<<<]>>>[-]+>>>>>>[>>>>>>>>>]>[-]+<]]+>[-<[>>>>>>>>>]<<<<<<<<]>
>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>->>[-<<<<+>>>>]<<<<[->>>>+<<[-]<<]>>]<<+>>>>[-<<<<
->>>>]+<<<<[->>>>-<<<<<<.>>]>>>>[-<<<<<<<.>>>>>>>]<<<[-]>[-]>[-]>[-]>[-]>[-]>>>[
>[-]>[-]>[-]>[-]>[-]>[-]>>>]<<<<<<<<<[<<<<<<<<<]>>>>>>>>>[>>>>>[-]>>>>]<<<<<<<<<
[<<<<<<<<<]>+++++++++++[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>+>>>>>>>>>+<<<<<<<<
<<<<<<[<<<<<<<<<]>>>>>>>[-<<<<<<<+>>>>>>>]<<<<<<<[->>>>>>>+[-]>>[>>>>>>>>>]<<<<<
<<<<[>>>>>>>[-<<<<<<+>>>>>>]<<<<<<[->>>>>>+<<<<<<<[<<<<<<<<<]>>>>>>>[-]+>>>]<<<<
<<<<<<]]>>>>>>>[-<<<<<<<+>>>>>>>]<<<<<<<[->>>>>>>+>>[>+>>>>[-<<<<->>>>]<<<<[->>>
>+<<<<]>>>>>>>>]<<+<<<<<<<[>>>>>[->>+<<]<<<<<<<<<<<<<<]>>>>>>>>>[>>>>>>>>>]<<<<<
<<<<[>[-]<->>>>>>>[-<<<<<<<+>[<->-<<<+>>>]<[->+<]>>>>>>>]<<<<<<[->>>>>>+<<<<<<]<
+<<<<<<<<<]>>>>>>>-<<<<[-]+<<<]+>>>>>>>[-<<<<<<<->>>>>>>]+<<<<<<<[->>>>>>>->>[>>
>>>[->>+<<]>>>>]<<<<<<<<<[>[-]<->>>>>>>[-<<<<<<<+>[<->-<<<+>>>]<[->+<]>>>>>>>]<<
<<<<[->>>>>>+<<<<<<]<+<<<<<<<<<]>+++++[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>+<<<
<<[<<<<<<<<<]>>>>>>>>>[>>>>>[-<<<<<->>>>>]+<<<<<[->>>>>->>[-<<<<<<<+>>>>>>>]<<<<
<<<[->>>>>>>+<<<<<<<<<<<<<<<<[<<<<<<<<<]>>>>[-]+>>>>>[>>>>>>>>>]>+<]]+>>>>>>>[-<
<<<<<<->>>>>>>]+<<<<<<<[->>>>>>>-<<[-<<<<<+>>>>>]<<<<<[->>>>>+<<<<<<<<<<<<<<[<<<
<<<<<<]>>>[-]+>>>>>>[>>>>>>>>>]>[-]+<]]+>[-<[>>>>>>>>>]<<<<<<<<]>>>>>>>>]<<<<<<<
<<[<<<<<<<<<]>>>>[-]<<<+++++[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>-<<<<<[<<<<<<<
<<]]>>>]<<<<.>>>>>>>>>>[>>>>>>[-]>>>]<<<<<<<<<[<<<<<<<<<]>++++++++++[-[->>>>>>>>
>+<<<<<<<<<]>>>>>>>>>]>>>>>+>>>>>>>>>+<<<<<<<<<<<<<<<[<<<<<<<<<]>>>>>>>>[-<<<<<<
<<+>>>>>>>>]<<<<<<<<[->>>>>>>>+[-]>[>>>>>>>>>]<<<<<<<<<[>>>>>>>>[-<<<<<<<+>>>>>>
>]<<<<<<<[->>>>>>>+<<<<<<<<[<<<<<<<<<]>>>>>>>>[-]+>>]<<<<<<<<<<]]>>>>>>>>[-<<<<<
<<<+>>>>>>>>]<<<<<<<<[->>>>>>>>+>[>+>>>>>[-<<<<<->>>>>]<<<<<[->>>>>+<<<<<]>>>>>>
>>]<+<<<<<<<<[>>>>>>[->>+<<]<<<<<<<<<<<<<<<]>>>>>>>>>[>>>>>>>>>]<<<<<<<<<[>[-]<-
>>>>>>>>[-<<<<<<<<+>[<->-<<+>>]<[->+<]>>>>>>>>]<<<<<<<[->>>>>>>+<<<<<<<]<+<<<<<<
<<<]>>>>>>>>-<<<<<[-]+<<<]+>>>>>>>>[-<<<<<<<<->>>>>>>>]+<<<<<<<<[->>>>>>>>->[>>>
>>>[->>+<<]>>>]<<<<<<<<<[>[-]<->>>>>>>>[-<<<<<<<<+>[<->-<<+>>]<[->+<]>>>>>>>>]<<
<<<<<[->>>>>>>+<<<<<<<]<+<<<<<<<<<]>+++++[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>>
+>>>>>>>>>>>>>>>>>>>>>>>>>>>+<<<<<<[<<<<<<<<<]>>>>>>>>>[>>>>>>[-<<<<<<->>>>>>]+<
<<<<<[->>>>>>->>[-<<<<<<<<+>>>>>>>>]<<<<<<<<[->>>>>>>>+<<<<<<<<<<<<<<<<<[<<<<<<<
<<]>>>>[-]+>>>>>[>>>>>>>>>]>+<]]+>>>>>>>>[-<<<<<<<<->>>>>>>>]+<<<<<<<<[->>>>>>>>
-<<[-<<<<<<+>>>>>>]<<<<<<[->>>>>>+<<<<<<<<<<<<<<<[<<<<<<<<<]>>>[-]+>>>>>>[>>>>>>
>>>]>[-]+<]]+>[-<[>>>>>>>>>]<<<<<<<<]>>>>>>>>]<<<<<<<<<[<<<<<<<<<]>>>>[-]<<<++++
+[-[->>>>>>>>>+<<<<<<<<<]>>>>>>>>>]>>>>>->>>>>>>>>>>>>>>>>>>>>>>>>>>-<<<<<<[<<<<
<<<<<]]>>>]
    "#.to_string();    
    remove_whitespace(&mut bfcode);

    println!("BrainfuckRVM: a Brainfuck Interpreter.\n");

    // Create a JIT cache
    let jit_cache = Arc::new(JitCache::new(1024 * 1024));

    let mut emu = Emu::new(30000).enable_jit(jit_cache);

    match emu.run(bfcode) {
        Some(VmExit::PtrOob) => {
            panic!("OOB")
        }
        Some(VmExit::Exit(elapsed)) => {
            println!("\nExecution time: [{:10.4}]s", elapsed);
            io::stdout().flush().ok().expect("Could not flush stdout");
        },
        _ => { unreachable!("something went wrong") }
    }

    println!("\ndone.");
    
}

fn hello_world() {
    let mut bfcode = r#"
    >++++++++[<+++++++++>-]<.>++++[<+++++++>-]<+.+++++++..+++.>>++++++[<+++++++>-]<+
+.------------.>++++++[<+++++++++>-]<+.<.+++.------.--------.>>>++++[<++++++++>-
]<+.
    "#.to_string();    
    remove_whitespace(&mut bfcode);

    println!("BrainfuckRVM: a Brainfuck Interpreter.\n");

    let mut emu = Emu::new(30000);
    match emu.run_vm3(bfcode) {
        Some(VmExit::PtrOob) => {
            panic!("OOB")
        }
        Some(VmExit::Exit(elapsed)) => {
            println!("\nExecution time: [{:10.4}]s", elapsed);
            io::stdout().flush().ok().expect("Could not flush stdout");
        },
        _ => { unreachable!("something went wrong") }
    }

    println!("\ndone.");
}

fn main() {
    plot_mandelbrot();
}