use std::{collections::BTreeMap, fmt};

#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
/// CPU affinity of worker threads.
///
/// Workers are placed on one logical CPU of each physical core first, spread
/// across NUMA nodes and shared L3 caches, and on the remaining SMT siblings
/// after that. On hybrid CPUs, performance cores are used before efficiency
/// cores. When there are more workers than CPUs, the placement starts over.
///
/// The default is [`Soft`](Self::Soft) on Windows and [`Core`](Self::Core) on
/// other platforms. Affinity is supported on Windows, Linux and Android, other
/// platforms ignore it.
pub enum Affinity {
    /// Worker threads are not pinned, the OS scheduler places them.
    Disabled,
    /// Pins each worker to one logical CPU.
    Cpu,
    /// Pins each worker to the logical CPUs of one physical core.
    ///
    /// The scheduler can move the worker between the SMT siblings of its
    /// core. Works like [`Cpu`](Self::Cpu) on CPUs without SMT.
    Core,
    /// Sets the preferred CPU of each worker without pinning it.
    ///
    /// On Windows, this sets the ideal processor of the worker thread. The
    /// scheduler runs the worker there when that CPU is free, and on another
    /// CPU otherwise. On machines with several processor groups, the worker is
    /// also kept in the group of its CPU. Other platforms have no such hint,
    /// there it works like [`Core`](Self::Core).
    Soft,
}

impl Default for Affinity {
    fn default() -> Self {
        if cfg!(windows) {
            Affinity::Soft
        } else {
            Affinity::Core
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Logical CPU, `group` is the Windows processor group.
pub(crate) struct Cpu {
    group: u16,
    num: u32,
}

impl fmt::Display for Cpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.group == 0 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}:{}", self.group, self.num)
        }
    }
}

#[derive(Copy, Clone, Debug)]
/// Topology of one logical CPU.
///
/// `core`, `l3` and `node` identify the physical core, L3 cache and NUMA
/// node. Higher `class` means a faster core. `allowed` is `false` if the
/// process may not use the CPU.
struct CpuInfo {
    cpu: Cpu,
    core: usize,
    l3: usize,
    node: usize,
    class: u32,
    allowed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// CPU placement of a worker thread.
pub(crate) struct Placement {
    mode: Affinity,
    cpu: Cpu,
    /// Logical CPUs of the physical core of `cpu` the process may use.
    core: Vec<Cpu>,
    /// All logical CPUs of the physical core of `cpu`.
    siblings: Vec<Cpu>,
}

impl Placement {
    /// Pins to the logical CPU with index `idx`.
    ///
    /// On Windows, CPUs are numbered across processor groups, the CPUs of
    /// group 1 follow the CPUs of group 0.
    pub(crate) fn cpu(idx: usize) -> Self {
        let cpu = sys::cpu(idx);
        Placement {
            mode: Affinity::Cpu,
            cpu,
            core: vec![cpu],
            siblings: vec![cpu],
        }
    }

    /// Applies the placement to the current thread.
    pub(crate) fn apply(&self) -> bool {
        sys::apply(self.mode, self.cpu, &self.core)
    }

    /// Returns the index of the placement CPU, followed by the indexes of
    /// its SMT siblings, including siblings the process may not use. The
    /// network stack may still process packets on them.
    pub(crate) fn cpus(&self) -> Vec<usize> {
        let mut cpus = vec![sys::index(self.cpu)];
        cpus.extend(
            self.siblings
                .iter()
                .filter(|cpu| **cpu != self.cpu)
                .map(|cpu| sys::index(*cpu)),
        );
        cpus
    }
}

/// Returns the index of the logical CPU that receives the packets of the
/// connection, if known.
///
/// On Windows, this is the receive-side scaling (RSS) CPU of the connection,
/// on Linux and Android the CPU that processed its last received packet
/// (`SO_INCOMING_CPU`). Not supported on other platforms.
pub(crate) fn incoming_cpu(stream: &std::net::TcpStream) -> Option<usize> {
    sys::incoming_cpu(stream).map(sys::index)
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} cpu {} (core", self.mode, self.cpu)?;
        for c in &self.core {
            write!(f, " {c}")?;
        }
        write!(f, ")")
    }
}

/// Returns worker placements in the order workers should use them.
///
/// Each logical CPU available to the process appears once. Empty if
/// affinity is disabled or not supported.
pub(crate) fn plan(mode: Affinity) -> Vec<Placement> {
    if mode == Affinity::Disabled {
        return Vec::new();
    }
    order(&sys::topology())
        .into_iter()
        .map(|(cpu, core, siblings)| Placement {
            mode,
            cpu,
            core,
            siblings,
        })
        .collect()
}

/// Returns the number of logical CPUs the process may use.
pub(crate) fn cpu_count() -> usize {
    // std counts only one processor group on Windows
    if cfg!(windows) {
        let n = sys::topology().iter().filter(|info| info.allowed).count();
        if n > 0 {
            return n;
        }
    }
    std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get)
}

struct Core {
    cpus: Vec<Cpu>,
    all: Vec<Cpu>,
    l3: usize,
    node: usize,
    class: u32,
}

/// Orders the logical CPUs the process may use, see [`Affinity`].
///
/// Returns each CPU with the CPUs of its physical core the process may use,
/// and all CPUs of that core.
fn order(cpus: &[CpuInfo]) -> Vec<(Cpu, Vec<Cpu>, Vec<Cpu>)> {
    let mut cores = BTreeMap::<usize, Core>::new();
    for info in cpus {
        let core = cores.entry(info.core).or_insert_with(|| Core {
            cpus: Vec::new(),
            all: Vec::new(),
            l3: info.l3,
            node: info.node,
            class: info.class,
        });
        core.all.push(info.cpu);
        if info.allowed {
            core.cpus.push(info.cpu);
        }
    }
    let mut cores: Vec<Core> = cores
        .into_values()
        .filter(|core| !core.cpus.is_empty())
        .collect();
    for core in &mut cores {
        core.cpus.sort_unstable();
        core.cpus.dedup();
        core.all.sort_unstable();
        core.all.dedup();
    }
    cores.sort_by_key(|core| core.cpus[0]);

    let mut classes: Vec<u32> = cores.iter().map(|core| core.class).collect();
    classes.sort_unstable_by(|a, b| b.cmp(a));
    classes.dedup();

    // cores of each class, interleaved across L3 caches within a NUMA node,
    // then across NUMA nodes
    let mut seq = Vec::with_capacity(cores.len());
    for class in classes {
        let mut nodes = BTreeMap::<usize, BTreeMap<usize, Vec<&Core>>>::new();
        for core in cores.iter().filter(|core| core.class == class) {
            nodes
                .entry(core.node)
                .or_default()
                .entry(core.l3)
                .or_default()
                .push(core);
        }
        seq.extend(interleave(
            nodes
                .into_values()
                .map(|l3| interleave(l3.into_values().collect()))
                .collect(),
        ));
    }

    // first logical CPU of every core, then the second one, and so on
    let depth = cores.iter().map(|core| core.cpus.len()).max().unwrap_or(0);
    let mut result = Vec::with_capacity(cpus.len());
    for idx in 0..depth {
        for core in &seq {
            if let Some(cpu) = core.cpus.get(idx) {
                result.push((*cpu, core.cpus.clone(), core.all.clone()));
            }
        }
    }
    result
}

/// Takes the first item of each list, then the second one, and so on.
fn interleave<T>(lists: Vec<Vec<T>>) -> Vec<T> {
    let mut iters: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut result = Vec::new();
    loop {
        let len = result.len();
        result.extend(iters.iter_mut().filter_map(Iterator::next));
        if result.len() == len {
            return result;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys {
    use std::{fs, mem, path::Path};

    use super::{Affinity, Cpu, CpuInfo};

    pub(super) fn topology() -> Vec<CpuInfo> {
        let root = Path::new("/sys/devices/system/cpu");
        let allowed = allowed();
        let mut cpus = list(&root.join("online")).unwrap_or_default();
        cpus.extend(&allowed);
        cpus.sort_unstable();
        cpus.dedup();
        read(root, &cpus, &allowed)
    }

    /// Reads the topology of `cpus` from sysfs at `root`, the process may
    /// use the `allowed` ones.
    pub(super) fn read(root: &Path, cpus: &[u32], allowed: &[u32]) -> Vec<CpuInfo> {
        cpus.iter()
            .map(|&num| {
                let dir = root.join(format!("cpu{num}"));
                let first = |files: &[&str]| {
                    files
                        .iter()
                        .find_map(|file| list(&dir.join(file))?.into_iter().min())
                        .unwrap_or(num) as usize
                };
                let node = fs::read_dir(&dir)
                    .ok()
                    .and_then(|entries| {
                        entries.flatten().find_map(|entry| {
                            entry
                                .file_name()
                                .to_str()?
                                .strip_prefix("node")?
                                .parse()
                                .ok()
                        })
                    })
                    .unwrap_or(0);
                let class = fs::read_to_string(dir.join("cpu_capacity"))
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);

                CpuInfo {
                    cpu: Cpu { group: 0, num },
                    core: first(&["topology/core_cpus_list", "topology/thread_siblings_list"]),
                    l3: first(&[
                        "cache/index3/shared_cpu_list",
                        "topology/package_cpus_list",
                        "topology/core_siblings_list",
                    ]),
                    node,
                    class,
                    allowed: allowed.contains(&num),
                }
            })
            .collect()
    }

    fn list(path: &Path) -> Option<Vec<u32>> {
        parse_list(&fs::read_to_string(path).ok()?)
    }

    /// Parses a CPU list like `0-3,8-11`.
    pub(super) fn parse_list(s: &str) -> Option<Vec<u32>> {
        let mut cpus = Vec::new();
        for item in s.trim().split(',').filter(|item| !item.is_empty()) {
            if let Some((from, to)) = item.split_once('-') {
                cpus.extend(from.parse::<u32>().ok()?..=to.parse().ok()?);
            } else {
                cpus.push(item.parse().ok()?);
            }
        }
        Some(cpus)
    }

    /// Returns the CPUs the current thread may run on.
    fn allowed() -> Vec<u32> {
        query_mask(|mask| {
            // SAFETY: the kernel writes at most `size_of_val(mask)` bytes,
            // the mask has the layout of a larger `cpu_set_t`
            let res = unsafe {
                libc::sched_getaffinity(0, mem::size_of_val(mask), mask.as_mut_ptr().cast())
            };
            if res == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            }
        })
        .unwrap_or_default()
    }

    /// Words of a `cpu_set_t`.
    const SET_WORDS: usize = mem::size_of::<libc::cpu_set_t>() / mem::size_of::<libc::c_ulong>();
    /// Largest mask tried, in words.
    const MAX_WORDS: usize = SET_WORDS << 10;

    /// Reads a CPU mask with `query`.
    ///
    /// The kernel mask may be larger than `cpu_set_t`, the query fails with
    /// `EINVAL` until the mask is large enough.
    pub(super) fn query_mask(
        mut query: impl FnMut(&mut [libc::c_ulong]) -> Result<(), i32>,
    ) -> Option<Vec<u32>> {
        let mut words = SET_WORDS;
        loop {
            let mut mask = vec![0; words];
            match query(&mut mask) {
                Ok(()) => {
                    let bits = libc::c_ulong::BITS;
                    return Some(
                        (0u32..)
                            .zip(&mask)
                            .flat_map(|(idx, word)| {
                                (0..bits)
                                    .filter(move |bit| word & (1 << bit) != 0)
                                    .map(move |bit| idx * bits + bit)
                            })
                            .collect(),
                    );
                }
                Err(libc::EINVAL) if words < MAX_WORDS => words *= 2,
                Err(_) => return None,
            }
        }
    }

    /// Returns a CPU mask of `cpus`, at least the size of `cpu_set_t`.
    pub(super) fn mask_of(cpus: &[Cpu]) -> Vec<libc::c_ulong> {
        let bits = libc::c_ulong::BITS;
        let max = cpus.iter().map(|cpu| cpu.num / bits + 1).max().unwrap_or(0);
        let mut mask = vec![0; SET_WORDS.max(max as usize)];
        for cpu in cpus {
            mask[(cpu.num / bits) as usize] |= 1 << (cpu.num % bits);
        }
        mask
    }

    pub(super) fn cpu(idx: usize) -> Cpu {
        Cpu {
            group: 0,
            num: u32::try_from(idx).unwrap_or(u32::MAX),
        }
    }

    pub(super) fn index(cpu: Cpu) -> usize {
        usize::try_from(cpu.num).unwrap_or(usize::MAX)
    }

    pub(super) fn incoming_cpu(stream: &std::net::TcpStream) -> Option<Cpu> {
        use std::os::fd::AsRawFd;

        #[cfg(target_os = "linux")]
        const SO_INCOMING_CPU: libc::c_int = libc::SO_INCOMING_CPU;
        #[cfg(target_os = "android")]
        const SO_INCOMING_CPU: libc::c_int = 49;

        let mut cpu: libc::c_int = -1;
        let mut len = libc::socklen_t::try_from(mem::size_of::<libc::c_int>()).ok()?;
        // SAFETY: the output buffer holds `len` bytes
        let res = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_INCOMING_CPU,
                (&raw mut cpu).cast(),
                &raw mut len,
            )
        };
        // -1 if no packet was received yet
        let num = u32::try_from(cpu).ok().filter(|_| res == 0)?;
        Some(Cpu { group: 0, num })
    }

    pub(super) fn apply(mode: Affinity, cpu: Cpu, core: &[Cpu]) -> bool {
        let cpus = if mode == Affinity::Cpu {
            std::slice::from_ref(&cpu)
        } else {
            core
        };
        if cpus
            .iter()
            .any(|cpu| cpu.num as usize >= MAX_WORDS * libc::c_ulong::BITS as usize)
        {
            return false;
        }
        let mask = mask_of(cpus);
        // SAFETY: the kernel reads `size_of_val(mask)` bytes, the mask has
        // the layout of a larger `cpu_set_t`
        unsafe {
            libc::sched_setaffinity(0, mem::size_of_val(&mask[..]), mask.as_ptr().cast()) == 0
        }
    }

    #[cfg(test)]
    /// Returns the CPUs the current thread may run on and its preferred CPU.
    pub(super) fn current() -> (Vec<Cpu>, Option<Cpu>) {
        let cpus = allowed()
            .into_iter()
            .map(|num| Cpu { group: 0, num })
            .collect();
        (cpus, None)
    }
}

#[cfg(windows)]
mod sys {
    use std::{mem, net::TcpStream, os::windows::io::AsRawSocket, ptr};

    use windows_sys::Win32::Networking::WinSock::{
        SIO_QUERY_RSS_PROCESSOR_INFO, SOCKET, SOCKET_PROCESSOR_AFFINITY, WSAIoctl,
    };
    use windows_sys::Win32::System::JobObjects::{
        JobObjectGroupInformationEx, QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Kernel::PROCESSOR_NUMBER;
    use windows_sys::Win32::System::SystemInformation::{
        CpuSetInformation, GROUP_AFFINITY, GetLogicalProcessorInformationEx,
        GetSystemCpuSetInformation, RelationAll, RelationCache, RelationNumaNode,
        RelationNumaNodeEx, RelationProcessorCore, SYSTEM_CPU_SET_INFORMATION,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    use windows_sys::Win32::System::Threading::{
        GetActiveProcessorCount, GetActiveProcessorGroupCount, GetCurrentProcess, GetCurrentThread,
        GetProcessAffinityMask, GetProcessDefaultCpuSets, GetProcessGroupAffinity,
        GetThreadGroupAffinity, SetThreadGroupAffinity, SetThreadIdealProcessorEx,
    };

    use super::{Affinity, Cpu, CpuInfo};

    #[allow(non_upper_case_globals)]
    pub(super) fn topology() -> Vec<CpuInfo> {
        let Some(buf) = query() else {
            return Vec::new();
        };

        let mut cpus = Vec::new();
        let mut l3 = Vec::new();
        let mut nodes = Vec::new();
        let base = buf.as_ptr();
        let len = buf.len() * mem::size_of::<u64>();
        let mut offset = 0;
        let mut idx = 0;

        // SAFETY: records are 8-byte aligned and lie within the buffer filled
        // by GetLogicalProcessorInformationEx, `GroupCount` masks follow
        // each record's mask field
        unsafe {
            while offset + mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= len {
                let rec = base
                    .byte_add(offset)
                    .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>();
                let size = (*rec).Size as usize;
                if size == 0 {
                    break;
                }
                let info = ptr::addr_of!((*rec).Anonymous);
                match (*rec).Relationship {
                    RelationProcessorCore => {
                        let p = ptr::addr_of!((*info).Processor);
                        let class = u32::from((*p).EfficiencyClass);
                        let masks = ptr::addr_of!((*p).GroupMask).cast::<GROUP_AFFINITY>();
                        for mask in masks_of(masks, (*p).GroupCount) {
                            cpus.extend(cpus_of(&mask).map(|cpu| CpuInfo {
                                cpu,
                                core: idx,
                                l3: usize::MAX,
                                node: usize::MAX - usize::from(cpu.group),
                                class,
                                allowed: true,
                            }));
                        }
                    }
                    RelationCache => {
                        let c = ptr::addr_of!((*info).Cache);
                        if (*c).Level == 3 {
                            let masks = ptr::addr_of!((*c).Anonymous).cast::<GROUP_AFFINITY>();
                            l3.extend(masks_of(masks, (*c).GroupCount).map(|m| (idx, m)));
                        }
                    }
                    RelationNumaNode | RelationNumaNodeEx => {
                        let n = ptr::addr_of!((*info).NumaNode);
                        let node = (*n).NodeNumber as usize;
                        let masks = ptr::addr_of!((*n).Anonymous).cast::<GROUP_AFFINITY>();
                        nodes.extend(masks_of(masks, (*n).GroupCount).map(|m| (node, m)));
                    }
                    _ => (),
                }
                offset += size;
                idx += 1;
            }
        }

        let contains = |mask: &GROUP_AFFINITY, cpu: Cpu| {
            mask.Group == cpu.group && cpu.num < usize::BITS && mask.Mask & (1 << cpu.num) != 0
        };
        for info in &mut cpus {
            if let Some((id, _)) = l3.iter().find(|(_, m)| contains(m, info.cpu)) {
                info.l3 = *id;
            } else {
                // no L3 cache, group by NUMA node
                info.l3 = usize::MAX - usize::from(info.cpu.group);
            }
            if let Some((id, _)) = nodes.iter().find(|(_, m)| contains(m, info.cpu)) {
                info.node = *id;
            }
        }

        let mask = affinity_mask();
        for info in &mut cpus {
            info.allowed = mask.is_none_or(|(group, mask)| {
                info.cpu.group == group
                    && info.cpu.num < usize::BITS
                    && mask & (1 << info.cpu.num) != 0
            });
        }
        if let Some(restricted) = restricted()
            && cpus
                .iter()
                .any(|info| info.allowed && restricted.contains(&info.cpu))
        {
            for info in &mut cpus {
                info.allowed &= restricted.contains(&info.cpu);
            }
        }
        cpus
    }

    /// Returns the CPUs allowed by the job object group affinity and the
    /// default CPU sets of the process, if any.
    ///
    /// The process affinity mask covers only one processor group.
    pub(super) fn restricted() -> Option<Vec<Cpu>> {
        let mut result: Option<Vec<Cpu>> = None;

        // SAFETY: buffers hold the lengths passed with them
        unsafe {
            let mut masks = [GROUP_AFFINITY::default(); 64];
            let mut len = 0u32;
            if QueryInformationJobObject(
                ptr::null_mut(),
                JobObjectGroupInformationEx,
                masks.as_mut_ptr().cast(),
                u32::try_from(mem::size_of_val(&masks)).unwrap_or(0),
                &raw mut len,
            ) != 0
            {
                let n = len as usize / mem::size_of::<GROUP_AFFINITY>();
                let cpus: Vec<Cpu> = masks[..n.min(masks.len())]
                    .iter()
                    .flat_map(cpus_of)
                    .collect();
                if !cpus.is_empty() {
                    result = Some(cpus);
                }
            }

            let process = GetCurrentProcess();
            let mut count = 0u32;
            GetProcessDefaultCpuSets(process, ptr::null_mut(), 0, &raw mut count);
            let mut ids = vec![0u32; count as usize];
            if count > 0
                && GetProcessDefaultCpuSets(process, ids.as_mut_ptr(), count, &raw mut count) != 0
            {
                ids.truncate(count as usize);
                let sets: Vec<Cpu> = cpu_sets()
                    .into_iter()
                    .filter(|(id, cpu)| {
                        ids.contains(id) && result.as_ref().is_none_or(|r| r.contains(cpu))
                    })
                    .map(|(_, cpu)| cpu)
                    .collect();
                if !sets.is_empty() {
                    result = Some(sets);
                }
            }
        }
        result
    }

    /// Returns the id and CPU of each CPU set of the system.
    fn cpu_sets() -> Vec<(u32, Cpu)> {
        let mut result = Vec::new();
        // SAFETY: the buffer holds `len` bytes and is 8-byte aligned, records
        // lie within the buffer
        unsafe {
            let mut len = 0u32;
            GetSystemCpuSetInformation(ptr::null_mut(), 0, &raw mut len, ptr::null_mut(), 0);
            let mut buf = vec![0u64; (len as usize).div_ceil(mem::size_of::<u64>())];
            if GetSystemCpuSetInformation(
                buf.as_mut_ptr().cast(),
                len,
                &raw mut len,
                ptr::null_mut(),
                0,
            ) == 0
            {
                return result;
            }
            let base = buf.as_ptr();
            let mut offset = 0;
            while offset + mem::size_of::<SYSTEM_CPU_SET_INFORMATION>() <= len as usize {
                let rec = base.byte_add(offset).cast::<SYSTEM_CPU_SET_INFORMATION>();
                let size = (*rec).Size as usize;
                if size == 0 {
                    break;
                }
                if (*rec).Type == CpuSetInformation {
                    let set = ptr::addr_of!((*rec).Anonymous.CpuSet);
                    result.push((
                        (*set).Id,
                        Cpu {
                            group: (*set).Group,
                            num: u32::from((*set).LogicalProcessorIndex),
                        },
                    ));
                }
                offset += size;
            }
        }
        result
    }

    fn query() -> Option<Vec<u64>> {
        let mut len = 0u32;
        // SAFETY: the buffer holds `len` bytes and is 8-byte aligned
        unsafe {
            GetLogicalProcessorInformationEx(RelationAll, ptr::null_mut(), &raw mut len);
            for _ in 0..3 {
                let mut buf = vec![0u64; (len as usize).div_ceil(mem::size_of::<u64>())];
                if GetLogicalProcessorInformationEx(
                    RelationAll,
                    buf.as_mut_ptr().cast(),
                    &raw mut len,
                ) != 0
                {
                    buf.truncate((len as usize).div_ceil(mem::size_of::<u64>()));
                    return Some(buf);
                }
            }
        }
        None
    }

    /// Reads `count` group masks starting at `ptr`.
    ///
    /// Older Windows versions report zero for a single mask.
    unsafe fn masks_of(
        ptr: *const GROUP_AFFINITY,
        count: u16,
    ) -> impl Iterator<Item = GROUP_AFFINITY> {
        // SAFETY: the caller guarantees `count` masks at `ptr`
        (0..usize::from(count.max(1))).map(move |i| unsafe { ptr.add(i).read_unaligned() })
    }

    fn cpus_of(mask: &GROUP_AFFINITY) -> impl Iterator<Item = Cpu> {
        let (group, mask) = (mask.Group, mask.Mask);
        (0..usize::BITS)
            .filter(move |num| mask & (1 << num) != 0)
            .map(move |num| Cpu { group, num })
    }

    /// Returns the processor group and affinity mask of the process if the
    /// mask restricts it to part of its group.
    ///
    /// A process starts in one processor group, and its threads may move to
    /// other groups. The groups the process currently runs in do not restrict
    /// it, an explicit affinity mask (`start /affinity`, job object affinity
    /// limits) does.
    pub(super) fn affinity_mask() -> Option<(u16, usize)> {
        // SAFETY: plain process affinity queries, the buffer holds one group
        unsafe {
            let process = GetCurrentProcess();
            let (mut mask, mut system) = (0, 0);
            // zero masks if the process runs in several groups
            if GetProcessAffinityMask(process, &raw mut mask, &raw mut system) == 0
                || mask == 0
                || mask == system
            {
                return None;
            }
            let (mut count, mut group) = (1u16, 0u16);
            (GetProcessGroupAffinity(process, &raw mut count, &raw mut group) != 0)
                .then_some((group, mask))
        }
    }

    fn mask_of(cpus: &[Cpu], group: u16) -> usize {
        cpus.iter()
            .filter(|cpu| cpu.group == group && cpu.num < usize::BITS)
            .fold(0, |mask, cpu| mask | (1 << cpu.num))
    }

    pub(super) fn cpu(mut idx: usize) -> Cpu {
        // SAFETY: plain processor count queries
        unsafe {
            for group in 0..GetActiveProcessorGroupCount() {
                let n = GetActiveProcessorCount(group);
                match u32::try_from(idx) {
                    Ok(num) if num < n => return Cpu { group, num },
                    _ => idx -= n as usize,
                }
            }
        }
        // no such CPU, applying it fails
        Cpu {
            group: u16::MAX,
            num: 0,
        }
    }

    pub(super) fn index(cpu: Cpu) -> usize {
        // SAFETY: plain processor count queries
        let before: u32 = (0..cpu.group)
            .map(|group| unsafe { GetActiveProcessorCount(group) })
            .sum();
        (before + cpu.num) as usize
    }

    // sockets are pointer-sized, the output buffer has a few bytes
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn incoming_cpu(stream: &TcpStream) -> Option<Cpu> {
        let mut info = SOCKET_PROCESSOR_AFFINITY::default();
        let mut len = 0u32;
        // SAFETY: the output buffer holds the length passed with it, the
        // call completes synchronously without an overlapped structure
        let res = unsafe {
            WSAIoctl(
                stream.as_raw_socket() as SOCKET,
                SIO_QUERY_RSS_PROCESSOR_INFO,
                ptr::null(),
                0,
                (&raw mut info).cast(),
                mem::size_of::<SOCKET_PROCESSOR_AFFINITY>() as u32,
                &raw mut len,
                ptr::null_mut(),
                None,
            )
        };
        (res == 0 && len as usize >= mem::size_of::<SOCKET_PROCESSOR_AFFINITY>()).then_some(Cpu {
            group: info.Processor.Group,
            num: u32::from(info.Processor.Number),
        })
    }

    pub(super) fn apply(mode: Affinity, cpu: Cpu, core: &[Cpu]) -> bool {
        // SAFETY: plain thread affinity calls on the current thread
        unsafe {
            let thread = GetCurrentThread();
            let mut affinity = GROUP_AFFINITY {
                Mask: 0,
                Group: cpu.group,
                Reserved: [0; 3],
            };
            match mode {
                Affinity::Disabled => true,
                Affinity::Cpu | Affinity::Core => {
                    affinity.Mask = if mode == Affinity::Cpu {
                        mask_of(&[cpu], cpu.group)
                    } else {
                        mask_of(core, cpu.group)
                    };
                    SetThreadGroupAffinity(thread, &raw const affinity, ptr::null_mut()) != 0
                }
                Affinity::Soft => {
                    // the ideal processor must be in the thread's group
                    let mut current = GROUP_AFFINITY::default();
                    if GetThreadGroupAffinity(thread, &raw mut current) != 0
                        && current.Group != cpu.group
                    {
                        let n = GetActiveProcessorCount(cpu.group);
                        affinity.Mask = if n >= usize::BITS { usize::MAX } else { (1 << n) - 1 };
                        if SetThreadGroupAffinity(thread, &raw const affinity, ptr::null_mut()) == 0
                        {
                            return false;
                        }
                    }
                    let Ok(num) = u8::try_from(cpu.num) else {
                        return false;
                    };
                    let ideal = PROCESSOR_NUMBER {
                        Group: cpu.group,
                        Number: num,
                        Reserved: 0,
                    };
                    SetThreadIdealProcessorEx(thread, &raw const ideal, ptr::null_mut()) != 0
                }
            }
        }
    }

    #[cfg(test)]
    /// Returns the CPUs the current thread may run on and its preferred CPU.
    pub(super) fn current() -> (Vec<Cpu>, Option<Cpu>) {
        use windows_sys::Win32::System::Threading::GetThreadIdealProcessorEx;

        // SAFETY: plain thread affinity calls on the current thread
        unsafe {
            let thread = GetCurrentThread();
            let mut affinity = GROUP_AFFINITY::default();
            let cpus = if GetThreadGroupAffinity(thread, &raw mut affinity) != 0 {
                cpus_of(&affinity).collect()
            } else {
                Vec::new()
            };
            let mut ideal = PROCESSOR_NUMBER::default();
            let ideal = (GetThreadIdealProcessorEx(thread, &raw mut ideal) != 0).then_some(Cpu {
                group: ideal.Group,
                num: u32::from(ideal.Number),
            });
            (cpus, ideal)
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
mod sys {
    use super::{Affinity, Cpu, CpuInfo};

    pub(super) fn topology() -> Vec<CpuInfo> {
        Vec::new()
    }

    pub(super) fn cpu(idx: usize) -> Cpu {
        Cpu {
            group: 0,
            num: u32::try_from(idx).unwrap_or(u32::MAX),
        }
    }

    pub(super) fn index(cpu: Cpu) -> usize {
        usize::try_from(cpu.num).unwrap_or(usize::MAX)
    }

    pub(super) fn incoming_cpu(_: &std::net::TcpStream) -> Option<Cpu> {
        None
    }

    pub(super) fn apply(_: Affinity, _: Cpu, _: &[Cpu]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(num: u32) -> Cpu {
        Cpu { group: 0, num }
    }

    fn info(num: u32, core: usize, l3: usize, node: usize) -> CpuInfo {
        CpuInfo {
            cpu: cpu(num),
            core,
            l3,
            node,
            class: 0,
            allowed: true,
        }
    }

    fn nums(cpus: &[CpuInfo]) -> Vec<u32> {
        order(cpus).into_iter().map(|(cpu, ..)| cpu.num).collect()
    }

    #[test]
    fn order_smt() {
        // siblings numbered next to each other (Windows)
        let cpus: Vec<_> = (0..8).map(|n| info(n, n as usize / 2, 0, 0)).collect();
        assert_eq!(nums(&cpus), [0, 2, 4, 6, 1, 3, 5, 7]);
        assert_eq!(order(&cpus)[5].1, [cpu(2), cpu(3)]);

        // siblings numbered apart (Linux)
        let cpus: Vec<_> = (0..8).map(|n| info(n, n as usize % 4, 0, 0)).collect();
        assert_eq!(nums(&cpus), [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(order(&cpus)[5].1, [cpu(1), cpu(5)]);
    }

    #[test]
    fn order_cache_and_nodes() {
        // two L3 caches
        let cpus: Vec<_> = (0..8)
            .map(|n| info(n, n as usize, n as usize / 4, 0))
            .collect();
        assert_eq!(nums(&cpus), [0, 4, 1, 5, 2, 6, 3, 7]);

        // two NUMA nodes with two L3 caches each
        let cpus: Vec<_> = (0..8)
            .map(|n| info(n, n as usize, n as usize / 2, n as usize / 4))
            .collect();
        assert_eq!(nums(&cpus), [0, 4, 2, 6, 1, 5, 3, 7]);

        // uneven L3 caches
        let cpus: Vec<_> = (0..6)
            .map(|n| info(n, n as usize, usize::from(n > 3), 0))
            .collect();
        assert_eq!(nums(&cpus), [0, 4, 1, 5, 2, 3]);
    }

    #[test]
    fn order_hybrid() {
        // performance cores with SMT, efficiency cores without
        let mut cpus: Vec<_> = (0..4).map(|n| info(n, n as usize / 2, 0, 0)).collect();
        for c in &mut cpus {
            c.class = 1;
        }
        cpus.extend((4..6).map(|n| info(n, n as usize, 0, 0)));
        assert_eq!(nums(&cpus), [0, 2, 4, 5, 1, 3]);
    }

    #[test]
    fn order_restricted() {
        // the process may only use some CPUs
        let cpus = [
            info(1, 0, 0, 0),
            info(2, 1, 0, 0),
            info(3, 1, 0, 0),
            info(5, 2, 0, 0),
        ];
        assert_eq!(nums(&cpus), [1, 2, 5, 3]);
        assert_eq!(order(&cpus)[0].1, [cpu(1)]);
        assert_eq!(order(&cpus)[3].1, [cpu(2), cpu(3)]);
        assert_eq!(order(&[]), []);

        // siblings the process may not use are kept for locality only
        let mut cpus: Vec<_> = (0..8).map(|n| info(n, n as usize % 4, 0, 0)).collect();
        for c in &mut cpus {
            c.allowed = [4, 6].contains(&c.cpu.num);
        }
        let order = order(&cpus);
        assert_eq!(nums(&cpus), [4, 6]);
        assert_eq!(order[0].1, [cpu(4)]);
        assert_eq!(order[0].2, [cpu(0), cpu(4)]);
        assert_eq!(order[1].2, [cpu(2), cpu(6)]);
    }

    #[test]
    fn plan_system() {
        assert_eq!(plan(Affinity::Disabled), []);

        let plan = plan(Affinity::Core);
        let mut cpus: Vec<_> = plan
            .iter()
            .map(|p| {
                assert_eq!(p.mode, Affinity::Core);
                assert!(p.core.contains(&p.cpu));
                assert!(p.core.iter().all(|cpu| p.siblings.contains(cpu)));
                p.cpu
            })
            .collect();
        cpus.sort_unstable();
        cpus.dedup();
        assert_eq!(cpus.len(), plan.len());
        assert!(cpu_count() > 0);
        if cfg!(windows) {
            assert_eq!(cpu_count(), plan.len());
        }
        #[cfg(any(windows, target_os = "linux", target_os = "android"))]
        assert_ne!(plan, []);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(sys::current().0.len(), plan.len());
        #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
        assert_eq!(plan, []);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    #[test]
    fn apply_placement() {
        let Some(Placement { cpu, core, .. }) = plan(Affinity::Core).pop() else {
            return;
        };
        for mode in [Affinity::Cpu, Affinity::Core, Affinity::Soft] {
            let p = Placement {
                mode,
                cpu,
                core: core.clone(),
                siblings: core.clone(),
            };
            let (cpus, ideal) = std::thread::spawn(move || {
                assert!(p.apply());
                sys::current()
            })
            .join()
            .unwrap();
            match mode {
                Affinity::Cpu => assert_eq!(cpus, [cpu]),
                Affinity::Core => assert_eq!(cpus, core),
                _ if cfg!(windows) => assert_eq!(ideal, Some(cpu)),
                _ => assert_eq!(cpus, core),
            }
        }
    }

    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    #[test]
    fn apply_cpu_index() {
        let Some(cpu) = plan(Affinity::Core)
            .into_iter()
            .map(|p| p.cpu)
            .filter(|cpu| cpu.group == 0)
            .max()
        else {
            return;
        };
        let p = Placement::cpu(cpu.num as usize);
        assert_eq!(p.cpu, cpu);
        let (cpus, _) = std::thread::spawn(move || {
            assert!(p.apply());
            sys::current()
        })
        .join()
        .unwrap();
        assert_eq!(cpus, [cpu]);

        assert!(!Placement::cpu(usize::MAX >> 1).apply());
    }

    #[test]
    fn cpu_indexes() {
        for p in plan(Affinity::Core) {
            let cpus = p.cpus();
            assert_eq!(sys::cpu(cpus[0]), p.cpu);
            assert_eq!(cpus.len(), p.siblings.len());
            for idx in &cpus {
                assert!(p.siblings.contains(&sys::cpu(*idx)), "{p}: {cpus:?}");
            }
        }
    }

    #[test]
    fn incoming_cpu_loopback() {
        let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
        let (server, _) = lst.accept().unwrap();
        // loopback connections have no RSS CPU on Windows, if known the CPU
        // must exist
        for stream in [&client, &server] {
            let cpu = incoming_cpu(stream);
            if let Some(cpu) = cpu {
                assert_eq!(sys::index(sys::cpu(cpu)), cpu);
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            assert!(cpu.is_some());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn linux_sysfs() {
        use std::fs;

        assert_eq!(
            sys::parse_list("0-3,8-11\n"),
            Some(vec![0, 1, 2, 3, 8, 9, 10, 11])
        );
        assert_eq!(sys::parse_list("5"), Some(vec![5]));
        assert_eq!(sys::parse_list("\n"), Some(vec![]));
        assert_eq!(sys::parse_list("a-b"), None);

        // cpus 0-2 on node 1 share an L3 cache, cpu0 and cpu2 are siblings.
        // cpu3 has no topology files, it is a separate core on node 0
        let root = std::env::temp_dir().join(format!("ntex-affinity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (num, core) in [(0, "0,2"), (1, "1,3"), (2, "0,2")] {
            let dir = root.join(format!("cpu{num}"));
            fs::create_dir_all(dir.join("topology")).unwrap();
            fs::create_dir_all(dir.join("cache/index3")).unwrap();
            fs::create_dir_all(dir.join("node1")).unwrap();
            fs::write(dir.join("topology/core_cpus_list"), core).unwrap();
            fs::write(dir.join("cache/index3/shared_cpu_list"), "0-3\n").unwrap();
        }
        let cpus = sys::read(&root, &[0, 1, 2, 3], &[0, 1, 2, 3]);
        let restricted = sys::read(&root, &[0, 1, 2, 3], &[0, 1, 3]);
        fs::remove_dir_all(&root).unwrap();

        let ids: Vec<_> = cpus.iter().map(|c| (c.core, c.l3, c.node)).collect();
        assert_eq!(ids, [(0, 0, 1), (1, 0, 1), (0, 0, 1), (3, 3, 0)]);
        assert_eq!(nums(&cpus), [3, 0, 1, 2]);

        // cpu2 is not allowed, it remains a sibling of cpu0
        assert_eq!(nums(&restricted), [3, 0, 1]);
        let (_, core, siblings) = &order(&restricted)[1];
        assert_eq!(
            (&core[..], &siblings[..]),
            (&[cpu(0)][..], &[cpu(0), cpu(2)][..])
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn linux_large_mask() {
        let bits = libc::c_ulong::BITS;
        let mask = sys::mask_of(&[cpu(1), cpu(3000)]);
        assert_eq!(mask.len(), (3000 / bits + 1) as usize);
        assert_eq!(mask[0], 2);
        assert_eq!(mask[(3000 / bits) as usize], 1 << (3000 % bits));
        assert_eq!(
            sys::mask_of(&[cpu(0)]).len() * size_of::<libc::c_ulong>(),
            size_of::<libc::cpu_set_t>()
        );

        // the kernel mask is larger than `cpu_set_t`
        let mut sizes = Vec::new();
        let cpus = sys::query_mask(|m| {
            sizes.push(m.len());
            if m.len() * bits as usize <= 3000 {
                return Err(libc::EINVAL);
            }
            m[..mask.len()].copy_from_slice(&mask);
            Ok(())
        });
        assert_eq!(cpus, Some(vec![1, 3000]));
        assert_eq!(sizes.len(), 3, "{sizes:?}");
        assert_eq!(sys::query_mask(|_| Err(libc::EPERM)), None);
        assert_eq!(sys::query_mask(|_| Err(libc::EINVAL)), None);

        assert!(!Placement::cpu(usize::MAX >> 1).apply());
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_mask() {
        use windows_sys::Win32::System::Threading::{
            ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess,
            GetProcessAffinityMask, SetProcessAffinityMask,
        };

        const CHILD: &str = "NTEX_AFFINITY_MASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // without restrictions, all processor groups are used
            if sys::affinity_mask().is_none() && sys::restricted().is_none() {
                // SAFETY: plain processor count query
                let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
                assert_eq!(plan(Affinity::Core).len(), n as usize);
            }
            // the process mask is changed in a separate process
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["affinity::tests::windows_process_mask", "--exact"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        // SAFETY: plain process affinity calls on the current process
        unsafe {
            let process = GetCurrentProcess();
            let (mut mask, mut system) = (0, 0);
            assert_ne!(
                GetProcessAffinityMask(process, &raw mut mask, &raw mut system),
                0
            );
            if mask.count_ones() < 3 {
                return;
            }
            // the two highest CPUs of the group
            let high = usize::BITS - 1 - mask.leading_zeros();
            let next = usize::BITS - 1 - (mask & !(1 << high)).leading_zeros();
            let restricted = (1usize << high) | (1 << next);
            assert_ne!(SetProcessAffinityMask(process, restricted), 0);

            let (group, mask) = sys::affinity_mask().unwrap();
            assert_eq!(mask, restricted);
            let mut cpus: Vec<_> = plan(Affinity::Core).into_iter().map(|p| p.cpu).collect();
            cpus.sort_unstable();
            assert_eq!(cpus, [Cpu { group, num: next }, Cpu { group, num: high }]);
            assert_eq!(cpu_count(), 2);
        }
    }
}
