use std::{
    fmt,
    ops::Range,
    sync::{Arc, Mutex},
    task::Poll,
};

use bytes::{BufMut, Bytes, BytesMut};

use crate::crypto::{
    chacha::{Cipher, CipherKind},
    kdf,
};
use crate::lang::{
    common::Role,
    interpreter,
    mem::Heap,
    message::Message,
    spec::proteus::ProteusSpec,
    task::{Instruction, ReadNetLength, Task, TaskID, TaskProvider, TaskSet},
    types::{ConcreteFormat, Identifier},
};

#[derive(std::fmt::Debug)]
pub enum Error {
    ExecuteFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::ExecuteFailed => write!(f, "Failed to execute instruction"),
        }
    }
}

impl From<interpreter::Error> for String {
    fn from(e: interpreter::Error) -> Self {
        e.to_string()
    }
}

pub struct SendArgs {
    // Send these bytes.
    pub bytes: Bytes,
}

pub struct RecvArgs {
    // Receive this many bytes.
    pub len: Range<usize>,
    // Store the bytes at this addr on the heap.
    pub addr: Identifier,
}

pub enum NetOpOut {
    RecvApp(RecvArgs),
    SendNet(SendArgs),
    Close,
    Error(String),
}

pub enum NetOpIn {
    RecvNet(RecvArgs),
    SendApp(SendArgs),
    Close,
    Error(String),
}

struct Program {
    task: Task,
    next_ins_index: usize,
    bytes_heap: Heap<Bytes>,
    format_heap: Heap<ConcreteFormat>,
    message_heap: Heap<Message>,
    number_heap: Heap<u128>,
}

impl Program {
    fn new(task: Task) -> Self {
        Self {
            task,
            next_ins_index: 0,
            bytes_heap: Heap::new(),
            format_heap: Heap::new(),
            message_heap: Heap::new(),
            number_heap: Heap::new(),
        }
    }

    fn has_next_instruction(&self) -> bool {
        self.next_ins_index < self.task.ins.len()
    }

    fn execute_next_instruction(
        &mut self,
        interpreter: &mut Interpreter,
    ) -> Result<(), interpreter::Error> {
        match &self.task.ins[self.next_ins_index] {
            Instruction::ComputeLength(args) => {
                let msg = self.message_heap.get(&args.from_msg_heap_id).unwrap();
                let len = msg.len_suffix(&args.from_field_id);
                self.number_heap
                    .insert(args.to_heap_id.clone(), len as u128);
            }
            Instruction::ConcretizeFormat(args) => {
                let aformat = args.from_format.clone();

                // Get the fields that have dynamic lengths, and compute what the lengths
                // will be now that we should have the data for each field on the heap.
                let concrete_sizes: Vec<(Identifier, usize)> = aformat
                    .get_dynamic_arrays()
                    .iter()
                    .map(|id| (id.clone(), self.bytes_heap.get(&id).unwrap().len()))
                    .collect();

                // Now that we know the total size, we can allocate the full format block.
                let cformat = aformat.concretize(&concrete_sizes);

                // Store it for use by later instructions.
                self.format_heap.insert(args.to_heap_id.clone(), cformat);
            }
            Instruction::CreateMessage(args) => {
                // Create a message with an existing concrete format.
                let cformat = self.format_heap.remove(&args.from_format_heap_id).unwrap();
                let msg = Message::new(cformat).unwrap();

                // Store the message for use in later instructions.
                self.message_heap.insert(args.to_heap_id.clone(), msg);
            }
            Instruction::DecryptField(args) => {
                match interpreter.cipher.as_mut() {
                    Some(cipher) => {
                        // TODO way too much copying here :(
                        let msg = self.message_heap.get(&args.from_msg_heap_id).unwrap();
                        let ciphertext =
                            msg.get_field_bytes(&args.from_ciphertext_field_id).unwrap();
                        let mac = msg.get_field_bytes(&args.from_mac_field_id).unwrap();

                        let mut mac_fixed = [0u8; 16];
                        mac_fixed.copy_from_slice(&mac);

                        let plaintext = cipher.decrypt(&ciphertext, &mac_fixed);

                        let mut buf = BytesMut::with_capacity(plaintext.len());
                        buf.put_slice(&plaintext);
                        self.bytes_heap
                            .insert(args.to_plaintext_heap_id.clone(), buf.freeze());
                    }
                    None => panic!("No cipher for decryption"),
                }
            }
            Instruction::EncryptField(args) => match interpreter.cipher.as_mut() {
                Some(cipher) => {
                    let msg = self.message_heap.get(&args.from_msg_heap_id).unwrap();
                    let plaintext = msg.get_field_bytes(&args.from_field_id).unwrap();

                    let (ciphertext, mac) = cipher.encrypt(&plaintext);

                    let mut buf = BytesMut::with_capacity(ciphertext.len());
                    buf.put_slice(&ciphertext);
                    self.bytes_heap
                        .insert(args.to_ciphertext_heap_id.clone(), buf.freeze());

                    let mut buf = BytesMut::with_capacity(mac.len());
                    buf.put_slice(&mac);
                    self.bytes_heap
                        .insert(args.to_mac_heap_id.clone(), buf.freeze());
                }
                None => panic!("No cipher for encryption"),
            },
            Instruction::GenRandomBytes(_args) => {
                todo!()
            }
            Instruction::GetArrayBytes(args) => {
                let msg = self.message_heap.get(&args.from_msg_heap_id).unwrap();
                let bytes = msg.get_field_bytes(&args.from_field_id).unwrap();
                self.bytes_heap.insert(args.to_heap_id.clone(), bytes);
            }
            Instruction::GetNumericValue(args) => {
                let msg = self.message_heap.get(&args.from_msg_heap_id).unwrap();
                let num = msg.get_field_unsigned_numeric(&args.from_field_id).unwrap();
                self.number_heap.insert(args.to_heap_id.clone(), num);
            }
            Instruction::InitFixedSharedKey(args) => {
                let salt = "stupid stupid stupid";
                let skey = kdf::derive_key_256(args.password.as_str(), salt);

                let kind = match args.role {
                    Role::Client => CipherKind::Sender,
                    Role::Server => CipherKind::Receiver,
                };
                interpreter.cipher = Some(Cipher::new(skey, kind));
            }
            Instruction::ReadApp(args) => {
                let netop = NetOpOut::RecvApp(RecvArgs {
                    len: args.from_len.clone(),
                    addr: args.to_heap_id.clone(),
                });
                interpreter.next_netop_out = Some(netop);
            }
            Instruction::ReadNet(args) => {
                let len = match &args.from_len {
                    ReadNetLength::Identifier(id) => {
                        let num = self.number_heap.get(&id).unwrap();
                        let val = *num as usize;
                        Range {
                            start: val,
                            end: val + 1,
                        }
                    }
                    ReadNetLength::IdentifierMinus((id, sub)) => {
                        let num = self.number_heap.get(&id).unwrap();
                        let val = (*num as usize) - sub;
                        Range {
                            start: val,
                            end: val + 1,
                        }
                    }
                    ReadNetLength::Range(r) => r.clone(),
                };

                let netop = NetOpIn::RecvNet(RecvArgs {
                    len,
                    addr: args.to_heap_id.clone(),
                });
                interpreter.next_netop_in = Some(netop);
            }
            Instruction::SetArrayBytes(args) => {
                let bytes = self.bytes_heap.get(&args.from_heap_id).unwrap();
                let mut msg = self.message_heap.remove(&args.to_msg_heap_id).unwrap();
                msg.set_field_bytes(&args.to_field_id, &bytes).unwrap();
                self.message_heap.insert(args.to_msg_heap_id.clone(), msg);
            }
            Instruction::SetNumericValue(args) => {
                let val = self.number_heap.get(&args.from_heap_id).unwrap().clone();
                let mut msg = self.message_heap.remove(&args.to_msg_heap_id).unwrap();
                msg.set_field_unsigned_numeric(&args.to_field_id, val)
                    .unwrap();
                self.message_heap.insert(args.to_msg_heap_id.clone(), msg);
            }
            Instruction::WriteApp(args) => {
                let msg = self.message_heap.remove(&args.from_msg_heap_id).unwrap();
                let netop = NetOpIn::SendApp(SendArgs {
                    bytes: msg.into_inner_field(&args.from_field_id).unwrap(),
                });
                interpreter.next_netop_in = Some(netop);
            }
            Instruction::WriteNet(args) => {
                let msg = self.message_heap.remove(&args.from_msg_heap_id).unwrap();
                let netop = NetOpOut::SendNet(SendArgs {
                    bytes: msg.into_inner(),
                });
                interpreter.next_netop_out = Some(netop);
            }
        };

        self.next_ins_index += 1;

        Ok(())
    }

    fn store_bytes(&mut self, addr: Identifier, bytes: Bytes) {
        self.bytes_heap.insert(addr, bytes);
    }
}

struct Interpreter {
    spec: Box<dyn TaskProvider + Send + 'static>,
    cipher: Option<Cipher>,
    next_netop_out: Option<NetOpOut>,
    next_netop_in: Option<NetOpIn>,
    current_prog_out: Option<Program>,
    current_prog_in: Option<Program>,
    last_task_id: TaskID,
    wants_tasks: bool,
}

impl Interpreter {
    fn new(spec: Box<dyn TaskProvider + Send + 'static>) -> Self {
        let mut int = Self {
            spec,
            cipher: None,
            next_netop_out: None,
            next_netop_in: None,
            current_prog_out: None,
            current_prog_in: None,
            last_task_id: TaskID::default(),
            wants_tasks: true,
        };

        let mut init_prog = Program::new(int.spec.get_init_task());
        while init_prog.has_next_instruction() {
            init_prog.execute_next_instruction(&mut int);
        }
        int.last_task_id = init_prog.task.id;
        int
    }

    /// Loads task from the task provider. Panics if we already have a current
    /// task in/out, we receive another one from the provider, and the ID of the
    /// new task does not match that of the existing task.
    fn load_tasks(&mut self) {
        match self.spec.get_next_tasks(&self.last_task_id) {
            TaskSet::InTask(task) => Self::set_task(&mut self.current_prog_in, task),
            TaskSet::OutTask(task) => Self::set_task(&mut self.current_prog_out, task),
            TaskSet::InAndOutTasks(pair) => {
                Self::set_task(&mut self.current_prog_in, pair.in_task);
                Self::set_task(&mut self.current_prog_out, pair.out_task);
            }
        };
        self.wants_tasks = false;
    }

    /// Inserts the given new task into the old Option. Panics if the option
    /// is Some and its task id does not match the new task id.
    fn set_task(opt: &mut Option<Program>, new: Task) {
        match opt {
            Some(op) => {
                if op.task.id != new.id {
                    panic!("Cannot overwrite task")
                }
            }
            None => *opt = Some(Program::new(new)),
        };
    }

    /// Return the next incoming (app<-net) command we want the network protocol
    /// to run, or an error if the app<-net direction should block for now.
    fn next_net_cmd_in(&mut self) -> Result<NetOpIn, ()> {
        // TODO: refactor this and next_net_cmd_out.
        loop {
            if self.wants_tasks {
                self.load_tasks();
            }

            match self.current_prog_in.take() {
                Some(mut program) => {
                    while program.has_next_instruction() {
                        if let Err(e) = program.execute_next_instruction(self) {
                            self.next_netop_in = Some(NetOpIn::Error(e.into()));
                        };

                        if let Some(netop) = self.next_netop_in.take() {
                            self.current_prog_in = Some(program);
                            return Ok(netop);
                        }
                    }
                    self.last_task_id = program.task.id;
                    self.wants_tasks = true;
                }
                None => return Err(()),
            }
        }
    }

    /// Return the next outgoing (app->net) command we want the network protocol
    /// to run, or an error if the app->net direction should block for now.
    fn next_net_cmd_out(&mut self) -> Result<NetOpOut, ()> {
        // TODO: refactor this and next_net_cmd_in.
        loop {
            if self.wants_tasks {
                self.load_tasks();
            }

            match self.current_prog_out.take() {
                Some(mut program) => {
                    while program.has_next_instruction() {
                        if let Err(e) = program.execute_next_instruction(self) {
                            self.next_netop_out = Some(NetOpOut::Error(e.into()));
                        };

                        if let Some(netop) = self.next_netop_out.take() {
                            self.current_prog_out = Some(program);
                            return Ok(netop);
                        }
                    }
                    self.last_task_id = program.task.id;
                    self.wants_tasks = true;
                }
                None => return Err(()),
            }
        }
    }

    /// Store the given bytes on the heap at the given address.
    fn store_in(&mut self, addr: Identifier, bytes: Bytes) {
        if let Some(t) = self.current_prog_in.as_mut() {
            t.store_bytes(addr, bytes);
        }
    }

    fn store_out(&mut self, addr: Identifier, bytes: Bytes) {
        if let Some(t) = self.current_prog_out.as_mut() {
            t.store_bytes(addr, bytes);
        }
    }
}

/// Wraps the interpreter allowing us to safely share the internal interpreter
/// state across threads while concurrently running network commands.
#[derive(Clone)]
pub struct SharedAsyncInterpreter {
    // The interpreter is protected by a global interpreter lock.
    inner: Arc<Mutex<Interpreter>>,
}

impl SharedAsyncInterpreter {
    pub fn new(spec: ProteusSpec) -> SharedAsyncInterpreter {
        SharedAsyncInterpreter {
            inner: Arc::new(Mutex::new(Interpreter::new(Box::new(spec)))),
        }
    }

    pub async fn next_net_cmd_out(&mut self) -> NetOpOut {
        // Yield to the async runtime if we can't get the lock, or if the
        // interpreter is not wanting to execute a command yet.
        std::future::poll_fn(move |_| {
            let mut inner = match self.inner.try_lock() {
                Ok(inner) => inner,
                Err(_) => return Poll::Pending,
            };
            match inner.next_net_cmd_out() {
                Ok(cmd) => Poll::Ready(cmd),
                Err(_) => Poll::Pending,
            }
        })
        .await
    }

    pub async fn next_net_cmd_in(&mut self) -> NetOpIn {
        // Yield to the async runtime if we can't get the lock, or if the
        // interpreter is not wanting to execute a command yet.
        std::future::poll_fn(move |_| {
            let mut inner = match self.inner.try_lock() {
                Ok(inner) => inner,
                Err(_) => return Poll::Pending,
            };
            match inner.next_net_cmd_in() {
                Ok(cmd) => Poll::Ready(cmd),
                Err(_) => Poll::Pending,
            }
        })
        .await
    }

    pub async fn store_out(&mut self, addr: Identifier, bytes: Bytes) {
        // Yield to the async runtime if we can't get the lock, or if the
        // interpreter is not wanting to execute a command yet.
        std::future::poll_fn(move |_| match self.inner.try_lock() {
            Ok(mut inner) => Poll::Ready(inner.store_out(addr.clone(), bytes.clone())),
            Err(_) => Poll::Pending,
        })
        .await
    }

    pub async fn store_in(&mut self, addr: Identifier, bytes: Bytes) {
        // Yield to the async runtime if we can't get the lock, or if the
        // interpreter is not wanting to execute a command yet.
        std::future::poll_fn(move |_| match self.inner.try_lock() {
            Ok(mut inner) => Poll::Ready(inner.store_in(addr.clone(), bytes.clone())),
            Err(_) => Poll::Pending,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Buf, BufMut, BytesMut};
    use std::fs;

    use self::{basic::*, encrypted::*};
    use super::*;

    use crate::lang::common::Role;
    use crate::lang::task::*;
    use crate::lang::types::*;

    trait SpecTestHarness {
        fn get_task_providers(&self) -> Vec<Box<dyn TaskProvider + Send + 'static>>;
        fn read_app(&self, int: &mut Interpreter) -> Bytes;
        fn write_net(&self, int: &mut Interpreter, payload: Bytes);
        fn read_net(&self, int: &mut Interpreter) -> Bytes;
        fn write_app(&self, int: &mut Interpreter, payload: Bytes);
    }

    fn get_test_harnesses() -> Vec<Box<dyn SpecTestHarness + Send + 'static>> {
        vec![
            Box::new(LengthPayloadSpecHarness {}),
            Box::new(EncryptedLengthPayloadSpecHarness {}),
        ]
    }

    mod basic {
        use super::*;

        pub struct LengthPayloadSpecHarness {}

        pub struct LengthPayloadSpec {
            abs_format_out: AbstractFormat,
            abs_format_in1: AbstractFormat,
            abs_format_in2: AbstractFormat,
        }

        impl LengthPayloadSpec {
            pub fn new() -> Self {
                let abs_format_out: AbstractFormat = Format {
                    name: "DataMessageOut".id(),
                    fields: vec![
                        Field {
                            name: "length".id(),
                            dtype: PrimitiveArray(NumericType::U16.into(), 1).into(),
                        },
                        Field {
                            name: "payload".id(),
                            dtype: DynamicArray(UnaryOp::SizeOf("length".id())).into(),
                        },
                    ],
                }
                .into();

                let abs_format_in1: AbstractFormat = Format {
                    name: "DataMessageIn1".id(),
                    fields: vec![Field {
                        name: "length".id(),
                        dtype: PrimitiveArray(NumericType::U16.into(), 1).into(),
                    }],
                }
                .into();

                let abs_format_in2: AbstractFormat = Format {
                    name: "DataMessageIn2".id(),
                    fields: vec![Field {
                        name: "payload".id(),
                        dtype: DynamicArray(UnaryOp::SizeOf("length".id())).into(),
                    }],
                }
                .into();

                Self {
                    abs_format_out,
                    abs_format_in1,
                    abs_format_in2,
                }
            }
        }

        impl TaskProvider for LengthPayloadSpec {
            fn get_init_task(&self) -> Task {
                Task {
                    ins: vec![],
                    id: Default::default(),
                }
            }

            fn get_next_tasks(&self, _last_task: &TaskID) -> TaskSet {
                // Outgoing data forwarding direction.
                let out_task = Task {
                    ins: vec![
                        ReadAppArgs {
                            from_len: 1..u16::MAX as usize,
                            to_heap_id: "payload".id(),
                        }
                        .into(),
                        ConcretizeFormatArgs {
                            from_format: self.abs_format_out.clone(),
                            to_heap_id: "cformat".id(),
                        }
                        .into(),
                        CreateMessageArgs {
                            from_format_heap_id: "cformat".id(),
                            to_heap_id: "message".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "payload".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "payload".id(),
                        }
                        .into(),
                        ComputeLengthArgs {
                            from_msg_heap_id: "message".id(),
                            from_field_id: "length".id(),
                            to_heap_id: "length_value_on_heap".id(),
                        }
                        .into(),
                        SetNumericValueArgs {
                            from_heap_id: "length_value_on_heap".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "length".id(),
                        }
                        .into(),
                        WriteNetArgs {
                            from_msg_heap_id: "message".id(),
                        }
                        .into(),
                    ],
                    id: TaskID::default(),
                };

                // Incoming data forwarding direction.
                let in_task = Task {
                    ins: vec![
                        ReadNetArgs {
                            from_len: ReadNetLength::Range(2..3 as usize),
                            to_heap_id: "length".id(),
                        }
                        .into(),
                        ConcretizeFormatArgs {
                            from_format: self.abs_format_in1.clone(),
                            to_heap_id: "cformat1".id(),
                        }
                        .into(),
                        CreateMessageArgs {
                            from_format_heap_id: "cformat1".id(),
                            to_heap_id: "message_length_part".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "length".id(),
                            to_msg_heap_id: "message_length_part".id(),
                            to_field_id: "length".id(),
                        }
                        .into(),
                        GetNumericValueArgs {
                            from_msg_heap_id: "message_length_part".id(),
                            from_field_id: "length".id(),
                            to_heap_id: "payload_len_value".id(),
                        }
                        .into(),
                        ReadNetArgs {
                            from_len: ReadNetLength::Identifier("payload_len_value".id()),
                            to_heap_id: "payload".id(),
                        }
                        .into(),
                        ConcretizeFormatArgs {
                            from_format: self.abs_format_in2.clone(),
                            to_heap_id: "cformat2".id(),
                        }
                        .into(),
                        CreateMessageArgs {
                            from_format_heap_id: "cformat2".id(),
                            to_heap_id: "message_payload_part".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "payload".id(),
                            to_msg_heap_id: "message_payload_part".id(),
                            to_field_id: "payload".id(),
                        }
                        .into(),
                        WriteAppArgs {
                            from_msg_heap_id: "message_payload_part".id(),
                            from_field_id: "payload".id(),
                        }
                        .into(),
                    ],
                    id: TaskID::default(),
                };

                // Concurrently execute tasks for both data forwarding directions.
                TaskSet::InAndOutTasks(TaskPair { out_task, in_task })
            }
        }

        impl LengthPayloadSpecHarness {
            fn parse_simple_proteus_spec(&self) -> ProteusSpec {
                let filepath = "src/lang/parse/examples/simple.psf";
                let input = fs::read_to_string(filepath).expect("cannot read simple file");

                ProteusSpec::new(&input, Role::Client)
            }
        }

        impl SpecTestHarness for LengthPayloadSpecHarness {
            fn get_task_providers(&self) -> Vec<Box<dyn TaskProvider + Send + 'static>> {
                vec![
                    Box::new(LengthPayloadSpec::new()),
                    // Box::new(self.parse_simple_proteus_spec()),
                ]
            }

            fn read_app(&self, int: &mut Interpreter) -> Bytes {
                let args = match int.next_net_cmd_out().unwrap() {
                    NetOpOut::RecvApp(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                let payload = Bytes::from("When should I attack?");
                assert!(args.len.contains(&payload.len()));

                int.store_out(args.addr, payload.clone());
                payload
            }

            fn write_net(&self, int: &mut Interpreter, payload: Bytes) {
                let args = match int.next_net_cmd_out().unwrap() {
                    NetOpOut::SendNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                let mut msg = args.bytes.clone();
                assert_eq!(msg.len(), payload.len() + 2); // 2 for length field
                assert_eq!(msg[2..], payload[..]);

                let len = msg.get_u16();
                assert_eq!(len as usize, payload.len());
            }

            fn read_net(&self, int: &mut Interpreter) -> Bytes {
                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::RecvNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert!(args.len.contains(&2));
                let payload = Bytes::from("Attack at dawn!");
                let mut buf = BytesMut::new();
                buf.put_u16(payload.len() as u16);
                int.store_in(args.addr, buf.freeze());

                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::RecvNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert!(args.len.contains(&payload.len()));
                int.store_in(args.addr, payload.clone());
                payload
            }

            fn write_app(&self, int: &mut Interpreter, payload: Bytes) {
                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::SendApp(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert_eq!(args.bytes.len(), payload.len());
                assert_eq!(args.bytes[..], payload[..]);
            }
        }
    }

    mod encrypted {
        use super::*;

        pub struct EncryptedLengthPayloadSpec {
            abs_format_out: AbstractFormat,
            abs_format_in1: AbstractFormat,
            abs_format_in2: AbstractFormat,
        }

        impl EncryptedLengthPayloadSpec {
            pub fn new() -> Self {
                let abs_format_out: AbstractFormat = Format {
                    name: "DataMessageOut".id(),
                    fields: vec![
                        Field {
                            name: "length".id(),
                            dtype: PrimitiveArray(NumericType::U16.into(), 1).into(),
                        },
                        Field {
                            name: "length_mac".id(),
                            dtype: PrimitiveArray(NumericType::U8.into(), 16).into(),
                        },
                        Field {
                            name: "payload".id(),
                            dtype: DynamicArray(UnaryOp::SizeOf("length".id())).into(),
                        },
                        Field {
                            name: "payload_mac".id(),
                            dtype: PrimitiveArray(NumericType::U8.into(), 16).into(),
                        },
                    ],
                }
                .into();

                let abs_format_in1: AbstractFormat = Format {
                    name: "DataMessageIn1".id(),
                    fields: vec![
                        Field {
                            name: "length".id(),
                            dtype: PrimitiveArray(NumericType::U16.into(), 1).into(),
                        },
                        Field {
                            name: "length_mac".id(),
                            dtype: PrimitiveArray(NumericType::U8.into(), 16).into(),
                        },
                    ],
                }
                .into();

                let abs_format_in2: AbstractFormat = Format {
                    name: "DataMessageIn2".id(),
                    fields: vec![
                        Field {
                            name: "payload".id(),
                            dtype: DynamicArray(UnaryOp::SizeOf("length".id())).into(),
                        },
                        Field {
                            name: "payload_mac".id(),
                            dtype: PrimitiveArray(NumericType::U8.into(), 16).into(),
                        },
                    ],
                }
                .into();

                Self {
                    abs_format_out,
                    abs_format_in1,
                    abs_format_in2,
                }
            }
        }

        impl TaskProvider for EncryptedLengthPayloadSpec {
            fn get_init_task(&self) -> Task {
                let password = "hunter2";

                Task {
                    id: Default::default(),
                    ins: vec![InitFixedSharedKeyArgs {
                        password: password.to_string(),
                        role: Role::Client,
                    }
                    .into()],
                }
            }

            fn get_next_tasks(&self, _last_task: &TaskID) -> TaskSet {
                // Outgoing data forwarding direction.
                let out_task = Task {
                    ins: vec![
                        ReadAppArgs {
                            from_len: 1..(u16::MAX - 32) as usize,
                            to_heap_id: "payload".id(),
                        }
                        .into(),
                        ConcretizeFormatArgs {
                            from_format: self.abs_format_out.clone(),
                            to_heap_id: "cformat".id(),
                        }
                        .into(),
                        CreateMessageArgs {
                            from_format_heap_id: "cformat".id(),
                            to_heap_id: "message".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "payload".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "payload".id(),
                        }
                        .into(),
                        ComputeLengthArgs {
                            from_msg_heap_id: "message".id(),
                            from_field_id: "length_mac".id(),
                            to_heap_id: "length_value_on_heap".id(),
                        }
                        .into(),
                        SetNumericValueArgs {
                            from_heap_id: "length_value_on_heap".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "length".id(),
                        }
                        .into(),
                        EncryptFieldArgs {
                            from_msg_heap_id: "message".id(),
                            from_field_id: "length".id(),
                            to_ciphertext_heap_id: "enc_length_heap".id(),
                            to_mac_heap_id: "length_mac_heap".id(),
                        }
                        .into(),
                        EncryptFieldArgs {
                            from_msg_heap_id: "message".id(),
                            from_field_id: "payload".id(),
                            to_ciphertext_heap_id: "enc_payload_heap".id(),
                            to_mac_heap_id: "payload_mac_heap".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "enc_length_heap".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "length".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "enc_payload_heap".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "payload".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "length_mac_heap".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "length_mac".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "payload_mac_heap".id(),
                            to_msg_heap_id: "message".id(),
                            to_field_id: "payload_mac".id(),
                        }
                        .into(),
                        WriteNetArgs {
                            from_msg_heap_id: "message".id(),
                        }
                        .into(),
                    ],
                    id: TaskID::default(),
                };

                // Incoming data forwarding direction.
                let in_task = Task {
                    ins: vec![
                        ReadNetArgs {
                            from_len: ReadNetLength::Range(2..3 as usize),
                            to_heap_id: "length".id(),
                        }
                        .into(),
                        ReadNetArgs {
                            from_len: ReadNetLength::Range(16..17 as usize),
                            to_heap_id: "length_mac".id(),
                        }
                        .into(),
                        ConcretizeFormatArgs {
                            from_format: self.abs_format_in1.clone(),
                            to_heap_id: "cformat1".id(),
                        }
                        .into(),
                        CreateMessageArgs {
                            from_format_heap_id: "cformat1".id(),
                            to_heap_id: "message_length_part".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "length".id(),
                            to_msg_heap_id: "message_length_part".id(),
                            to_field_id: "length".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "length_mac".id(),
                            to_msg_heap_id: "message_length_part".id(),
                            to_field_id: "length_mac".id(),
                        }
                        .into(),
                        DecryptFieldArgs {
                            from_msg_heap_id: "message_length_part".id(),
                            from_ciphertext_field_id: "length".id(),
                            from_mac_field_id: "length_mac".id(),
                            to_plaintext_heap_id: "dec_length_heap".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "dec_length_heap".id(),
                            to_msg_heap_id: "message_length_part".id(),
                            to_field_id: "length".id(),
                        }
                        .into(),
                        GetNumericValueArgs {
                            from_msg_heap_id: "message_length_part".id(),
                            from_field_id: "length".id(),
                            to_heap_id: "payload_len_value_heap".id(),
                        }
                        .into(),
                        ReadNetArgs {
                            from_len: ReadNetLength::IdentifierMinus((
                                "payload_len_value_heap".id(),
                                16,
                            )),
                            to_heap_id: "payload".id(),
                        }
                        .into(),
                        ReadNetArgs {
                            from_len: ReadNetLength::Range(16..17 as usize),
                            to_heap_id: "payload_mac".id(),
                        }
                        .into(),
                        ConcretizeFormatArgs {
                            from_format: self.abs_format_in2.clone(),
                            to_heap_id: "cformat2".id(),
                        }
                        .into(),
                        CreateMessageArgs {
                            from_format_heap_id: "cformat2".id(),
                            to_heap_id: "message_payload_part".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "payload".id(),
                            to_msg_heap_id: "message_payload_part".id(),
                            to_field_id: "payload".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "payload_mac".id(),
                            to_msg_heap_id: "message_payload_part".id(),
                            to_field_id: "payload_mac".id(),
                        }
                        .into(),
                        DecryptFieldArgs {
                            from_msg_heap_id: "message_payload_part".id(),
                            from_ciphertext_field_id: "payload".id(),
                            from_mac_field_id: "payload_mac".id(),
                            to_plaintext_heap_id: "dec_payload_heap".id(),
                        }
                        .into(),
                        SetArrayBytesArgs {
                            from_heap_id: "dec_payload_heap".id(),
                            to_msg_heap_id: "message_payload_part".id(),
                            to_field_id: "payload".id(),
                        }
                        .into(),
                        WriteAppArgs {
                            from_msg_heap_id: "message_payload_part".id(),
                            from_field_id: "payload".id(),
                        }
                        .into(),
                    ],
                    id: TaskID::default(),
                };

                // Concurrently execute tasks for both data forwarding directions.
                TaskSet::InAndOutTasks(TaskPair { out_task, in_task })
            }
        }

        pub struct EncryptedLengthPayloadSpecHarness {}

        impl EncryptedLengthPayloadSpecHarness {
            fn _parse_encrypted_proteus_spec(&self) -> ProteusSpec {
                let filepath = "src/lang/parse/examples/encrypted.psf";
                let input = fs::read_to_string(filepath).expect("cannot read encrypted file");

                ProteusSpec::new(&input, Role::Client)
            }
        }

        impl SpecTestHarness for EncryptedLengthPayloadSpecHarness {
            fn get_task_providers(&self) -> Vec<Box<dyn TaskProvider + Send + 'static>> {
                vec![
                    Box::new(EncryptedLengthPayloadSpec::new()),
                    // Box::new(self.parse_encrypted_proteus_spec()),
                ]
            }

            fn read_app(&self, int: &mut Interpreter) -> Bytes {
                let args = match int.next_net_cmd_out().unwrap() {
                    NetOpOut::RecvApp(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                let payload = Bytes::from("When should I attack?");
                assert!(args.len.contains(&payload.len()));

                int.store_out(args.addr, payload.clone());
                payload
            }

            fn write_net(&self, int: &mut Interpreter, payload: Bytes) {
                let args = match int.next_net_cmd_out().unwrap() {
                    NetOpOut::SendNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                let mut msg = args.bytes.clone();
                assert_eq!(msg.len(), payload.len() + 2 + 16 + 16); // len and 2 macs
                assert_eq!(msg[18..(msg.len() - 16)], payload[..]);

                let len = msg.get_u16();
                assert_eq!(len as usize, payload.len() + 16); // mac
            }

            fn read_net(&self, int: &mut Interpreter) -> Bytes {
                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::RecvNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert!(args.len.contains(&2));

                let payload = Bytes::from("Attack at dawn!");
                let mac = Bytes::from_static(&[0; 16]);

                let mut buf = BytesMut::new();
                buf.put_u16((payload.len() + mac.len()) as u16);
                assert!(args.len.contains(&buf.len()));
                int.store_in(args.addr, buf.freeze());

                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::RecvNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert!(args.len.contains(&mac.len()));
                int.store_in(args.addr, mac.clone());

                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::RecvNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert!(args.len.contains(&payload.len()));
                int.store_in(args.addr, payload.clone());

                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::RecvNet(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert!(args.len.contains(&mac.len()));
                int.store_in(args.addr, mac.clone());
                payload
            }

            fn write_app(&self, int: &mut Interpreter, payload: Bytes) {
                let args = match int.next_net_cmd_in().unwrap() {
                    NetOpIn::SendApp(args) => args,
                    _ => panic!("Unexpected interpreter command"),
                };

                assert_eq!(args.bytes.len(), payload.len());
                assert_eq!(args.bytes[..], payload[..]);
            }
        }
    }

    #[test]
    fn load_tasks() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                int.load_tasks();
                assert!(int.current_prog_in.is_some() || int.current_prog_out.is_some());
            }
        }
    }

    fn read_app_write_net_pipeline(th: &Box<dyn SpecTestHarness + Send>, int: &mut Interpreter) {
        let payload = th.read_app(int);
        th.write_net(int, payload);
    }

    #[test]
    fn read_app_write_net_once() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                read_app_write_net_pipeline(&th, &mut int);
            }
        }
    }

    #[test]
    fn read_app_write_net_many() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                for _ in 0..10 {
                    read_app_write_net_pipeline(&th, &mut int);
                }
            }
        }
    }

    fn read_net_write_app_pipeline(th: &Box<dyn SpecTestHarness + Send>, int: &mut Interpreter) {
        let payload = th.read_net(int);
        th.write_app(int, payload);
    }

    #[test]
    fn read_net_write_app_once() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                read_net_write_app_pipeline(&th, &mut int);
            }
        }
    }

    #[test]
    fn read_net_write_app_many() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                for _ in 0..10 {
                    read_net_write_app_pipeline(&th, &mut int);
                }
            }
        }
    }

    #[test]
    fn interleaved_app_net_app_net() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                for _ in 0..10 {
                    let app_payload = th.read_app(&mut int);
                    let net_payload = th.read_net(&mut int);
                    th.write_app(&mut int, net_payload);
                    th.write_net(&mut int, app_payload);
                }
            }
        }
    }

    #[test]
    fn interleaved_net_app_net_app() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                for _ in 0..10 {
                    let net_payload = th.read_net(&mut int);
                    let app_payload = th.read_app(&mut int);
                    th.write_net(&mut int, app_payload);
                    th.write_app(&mut int, net_payload);
                }
            }
        }
    }

    #[test]
    fn interleaved_app_net_net_app() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                for _ in 0..10 {
                    let app_payload = th.read_app(&mut int);
                    let net_payload = th.read_net(&mut int);
                    th.write_net(&mut int, app_payload);
                    th.write_app(&mut int, net_payload);
                }
            }
        }
    }

    #[test]
    fn interleaved_net_app_app_net() {
        for th in get_test_harnesses() {
            for tp in th.get_task_providers() {
                let mut int = Interpreter::new(tp);
                for _ in 0..10 {
                    let net_payload = th.read_net(&mut int);
                    let app_payload = th.read_app(&mut int);
                    th.write_app(&mut int, net_payload);
                    th.write_net(&mut int, app_payload);
                }
            }
        }
    }
}
