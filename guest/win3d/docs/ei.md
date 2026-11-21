# ExecuteIndirect with state-changing command signatures

Two patches make this work. `patches/vkd3d-proton/0006` handles command
signatures that set root constants and root descriptors, and is what the
first sections describe. `patches/vkd3d-proton/0007` handles signatures that
set vertex and index buffer views
([below](#vertex-and-index-buffer-views)).

`D3D12ExecuteIndirect`, one of Microsoft's DirectX-Graphics-Samples, drew
only its clear colour on AMD1 and Nvidia1 ([d3d12app.md](d3d12app.md)).
`patches/vkd3d-proton/0006` fixes this. Without `VK_EXT_device_generated_commands`,
vkd3d-proton now emulates `ExecuteIndirect` for command signatures that
change root constants or root descriptors. The fix changes nothing on the
host and nothing in Venus. The work was done on 2026-10-19 on helios: RADV
(Mesa 26.2.2) on `AMD1`, NVIDIA 610.57.04 on `Nvidia1` and `Nvidia2`,
virglrenderer 1.3.0, the package built from [d6f52e4](https://github.com/VMLabDev/vmlab-win3d/commit/d6f52e40fe0acedb89eddf0456a3b0ba69541b79). The evidence is in
[docs/evidence/ei/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/ei/).

## Result

| Check | AMD1 | Nvidia1 | Nvidia2 |
| --- | --- | --- | --- |
| Repro `ei_repro`: the GPU against WARP, and both against the expected values | pass, 0 of 16384 pixels differ | pass, 0 of 16384 | pass, 0 of 16384 |
| `D3D12ExecuteIndirect`, unmodified, against the same sample on WARP | pass, coverage ratio 1.13 | pass, coverage ratio 1.12 | not run |
| The five samples (`tests/apps/d3d12/run.sh`): exit 0, no app-local module | 5 of 5 | 5 of 5 | not run |
| `tests/e2e/run.sh --template windows-11` | 12 of 12 | 12 of 12 | not run |

The Nvidia2 repro ran before Nvidia2 was reserved for another worker. The
sample, e2e and second repro runs then moved to Nvidia1.

## Cause

`D3D12ExecuteIndirect` builds a command signature with a root
constant-buffer view argument followed by a draw. vkd3d-proton implements
any signature that changes state with `VK_EXT_device_generated_commands`
(DGC). RADV and the NVIDIA driver have DGC on the host, but Venus does not
forward it. Without DGC, `d3d12_command_signature_create` dropped the state
arguments, and `ExecuteIndirect` returned without drawing
([d3d12app.md](d3d12app.md#executeindirect-draws-nothing) has the trace).

## Choosing the fix

The brief offered two fixes.

**A: DGC through Venus.** Venus encodes every Vulkan command in a protocol
that `venus-protocol` generates from `vk.xml`. Its current tree
(`gitlab.freedesktop.org/virgl/venus-protocol`, commit ca19b63 of 2026-09-06)
lists no device-generated-commands extension. Venus in newer Mesa and
virglrenderer therefore has no support to backport. This fix would need:

- a protocol extension, with two new object types: indirect commands layouts
  and indirect execution sets;
- the guest driver's object tracking for those types;
- the host renderer's dispatch for them.

It would also work only with our own `virgl_render_server`, never with a
stock host.

**B: emulation in vkd3d-proton.** This needs one component, which we
already patch, and it works on any host. B was chosen.

## The fix

Patch `0006` adds an emulation path, used only when the device has no DGC.
The path has three parts.

### Root signatures

Every root signature with root parameters puts them in vkd3d-proton's push
UBO block (`VKD3D_ROOT_SIGNATURE_USE_PUSH_CONSTANT_UNIFORM_BLOCK`), instead
of push constants. Root CBVs become raw VAs, as root SRVs and UAVs already
are. vkd3d-proton already takes this path when root parameters exceed
`maxPushConstantsSize`. The shader reads root constants and root descriptor
addresses from memory, so the GPU can write them per draw. The root
signature is created before anything shows whether it will be used with
`ExecuteIndirect`, so the choice applies to every root signature.

### Command signatures

A signature that changes root constants or root CBV, SRV or UAV addresses,
and ends in a draw, indexed draw, mesh draw or dispatch, gets a patch list.
For each argument word, the list names its source offset in the
application's argument record and its destination in a per-sequence slot:

- **Root constants:** the constant's word in the root parameter block.
- **Root descriptors:** the two words of the raw VA.
- **Incrementing constants:** no source word. The CPU writes the sequence
  index into each slot.
- **The action's arguments:** placed after the root parameter block, 16-byte
  aligned.

A slot is a multiple of 256 bytes, so every slot is a valid UBO offset.

### ExecuteIndirect

When a command list records `ExecuteIndirect` with such a signature, it:

1. allocates `MaxCommandCount` slots in upload memory;
2. records vkd3d-proton's existing patch shader
   (`cs_execute_indirect_patch.comp`), which copies the argument buffer into
   the slots and stops at the count buffer's value. This runs in the command
   list's own order, outside the render pass, followed by a barrier to
   indirect and uniform reads. Under fallback predication, the predicate is
   folded into the count;
3. flushes the bound state as for a normal draw or dispatch;
4. fills every slot on the CPU with the currently bound root parameters and
   zeroed action arguments. The GPU patch then overwrites the words the
   signature sets. Sequences past the count keep zero arguments and draw or
   dispatch nothing;
5. for each sequence, pushes the slot as the root UBO and records one
   `vkCmdDrawIndirect`, `vkCmdDrawIndexedIndirect`,
   `vkCmdDrawMeshTasksIndirectEXT` or `vkCmdDispatchIndirect` reading the
   slot;
6. clears the state that the signature set, as D3D12 requires, and marks all
   state dirty.

vkd3d-proton logs one line per emulated signature:

```
info:d3d12_command_signature_init_state_template_emulated: Emulating ExecuteIndirect state (10 patch words, slot stride 256) without EXT_dgc.
```

## Evidence

All under [docs/evidence/ei/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/ei/):

| Path | What |
| --- | --- |
| `src/ei_repro.cpp`, `src/build.sh` | The repro and its build in the shared toolchain image |
| `src/run-repro.sh` | Runs the repro in a lab: GPU, WARP, compare |
| `src/sample-warp.sh`, `src/compare-sample.py` | Runs the unmodified sample on WARP and compares its console capture with the GPU run's |
| `amd1-repro/`, `nvidia1-repro/`, `nvidia2-repro/` | `repro.txt` (the three outputs), `ei_gpu.png`, `ei_warp.png`, and the vkd3d-proton log with the emulation lines |
| `samples-amd1/`, `samples-nvidia1/` | The five samples (`samples.json`, captures, run and module lists, DXVK logs, `app-errors.txt`), and the WARP run of `D3D12ExecuteIndirect`: `console-D3D12ExecuteIndirect-warp.png`, `sample-compare.txt` |
| `e2e/amd1/`, `e2e/nvidia1/` | `tests/e2e/run.sh` evidence; `results.txt` ends `PASS: 12 of 12`, and `build.txt` shows the package reused from [d6f52e4](https://github.com/VMLabDev/vmlab-win3d/commit/d6f52e40fe0acedb89eddf0456a3b0ba69541b79) |

### The repro

`ei_repro` records two `ExecuteIndirect` calls in one command list:

- **Graphics:** a signature of root constants (colour), a root CBV (cell
  offset) and a draw. It records 16 sequences on a 4×4 grid, with a count
  buffer of 13. A root constant outside the signature (the quad size) is set
  on the CPU, and the signature's own parameters are first set to wrong
  values that `ExecuteIndirect` must replace.
- **Compute:** a signature of a root constant and a dispatch, with 8
  sequences of 4 groups writing to a root UAV.

`ei_repro gpu` runs on the default adapter through the package's
`d3d12.dll`. `ei_repro warp` runs in a second process, from a directory
holding Microsoft's `d3d12.dll` (the package keeps it as
`System32\vmlab-win3d\d3d12-native.dll`) and `System32\D3D12Core.dll`.
Outside System32, the stock `d3d12.dll` looks for its core next to itself,
and fails with `0x887E0003` otherwise. `ei_repro compare` checks the 16 cell
centres and 64 UAV words of each run against the expected values, and then
the two images against each other. Each check allows 1/255 per channel: on
NVIDIA, colour 0.5 converts to 127, while WARP and RADV give 128.

### The sample on WARP

The sample animates 1024 triangles that start at `rand()` positions, and
its compute pass culls those outside the middle of the window. Two captures
are never of the same frame, so `compare-sample.py` compares what they must
share:

- the client area, found by the clear colour;
- the share of that area the triangles cover (within a factor of 1.5);
- how that coverage spreads across eight vertical bands (each band within
  0.1).

The samples export `D3D12SDKVersion = 618` and `D3D12SDKPath = .\D3D12\`.
Microsoft's `d3d12.dll` running from outside System32 needs the matching
Agility SDK core there, so `sample-warp.sh` deploys `D3D12Core.dll` from the
pinned `Microsoft.Direct3D.D3D12` 1.618.3 package that
`build/d3d12-samples/` downloads. Without it, every sample on WARP fails with
`0xC0000409`. The before capture
([docs/evidence/d3d12app/amd1/console-D3D12ExecuteIndirect.png](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/d3d12app/amd1/console-D3D12ExecuteIndirect.png)) scores a
coverage of 0.000. After the fix, the GPU covers 0.202 on AMD1 and 0.206 on
Nvidia1, and WARP covers 0.178 and 0.183.

### Other observations

- In the AMD1 sample lab, `dwm.exe` crashed once in `viogpu_d3d10.dll`
  (`0xc000041d`) during `D3D12HelloTexture`, in the first boot after the
  install (`samples-amd1/app-errors.txt`). Every sample still exited 0 and
  rendered. This looks like the first-boot dwm crash in
  [d3d12app.md](d3d12app.md#nvidia-dwm-crashes-in-the-first-boot-after-the-install),
  which until now had been seen only on NVIDIA. This patch does not touch it.
- Labs from the `windows-11` template report "Windows License is expired"
  and shut down about an hour after boot, even with a NAT NIC.

## Vertex and index buffer views

`0006` left out signatures with `D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW`
or `D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW` arguments. vkd3d-proton
then dropped the view arguments, and each sequence drew with the buffers
bound on the CPU: wrong drawing with no error. `patches/vkd3d-proton/0007`
emulates them. The work was done on 2026-11-21 on helios (card 254).

### Why the views cannot be bound

Root constants and root descriptor addresses are data, so the GPU can write
them into a slot. A vertex or index buffer is different. Vulkan binds it with
`vkCmdBindVertexBuffers2` or `vkCmdBindIndexBuffer`, by `VkBuffer` handle and
offset, while the command buffer is recorded. The view in a sequence is an
address the GPU reads when the command buffer runs, and it can name any
buffer of the application at any offset. Only the first vertex, the first
index and the vertex offset of an indirect draw come from the GPU.
`VK_EXT_device_generated_commands` has tokens that bind by address, which is
what vkd3d-proton uses when the device has it.

### The vertex fetch

`0007` does not bind the application's buffers. It copies what each sequence
draws into scratch buffers, binds those for every sequence, and rewrites the
draw arguments to point there. Each sequence stays one indirect draw, as in
`0006`, so root arguments in the same signature keep working.

A new compute shader, `cs_execute_indirect_vertex_fetch.comp`, runs up to
four passes before the draws. They are recorded in the command list's own
order, outside the render pass:

1. **Index layout** (one invocation, indexed draws only): reads every
   sequence's `IndexCountPerInstance` and gives it a range of the index
   scratch buffer.
2. **Index scan** (one invocation per index): reads the index through the
   sequence's index buffer view, or through the view bound on the CPU when
   the signature has none. It stores the index as 32 bits and keeps the lowest
   and highest index of the sequence with atomic minimum and maximum. A strip
   cut index is stored as `0xFFFFFFFF` and is not counted.
3. **Vertex layout** (one invocation): gives every sequence a range of the
   vertex scratch buffers, as long as the range of vertices it uses, and a
   range of the instance scratch buffers, as long as its `InstanceCount`. It
   then writes the sequence's draw arguments into its slot: the first index,
   and a vertex offset that maps the lowest index to the start of the range.
4. **Copy** (one invocation per vertex or instance element): copies the
   element of every fetched binding from the address in the view. The view's
   own stride and size are used, and bytes outside the view read as 0.

The index values are not changed, so `SV_VertexID` is what Direct3D defines
(the index, without `BaseVertexLocation`). `SV_InstanceID` is unchanged too.

One draw gives all per-vertex bindings the same first vertex, and all
per-instance bindings the same first instance. So when the signature sets one
binding of a rate, the other bindings of that rate that the pipeline uses are
fetched with it, from the views bound on the CPU. A signature that sets only
the index buffer fetches indices and leaves the vertex buffers bound. For
dispatch and mesh signatures the view arguments have no effect, as in
Direct3D.

A signature with view arguments and no root arguments has no root signature.
Its slots then hold only the draw arguments, and the root parameters that
are bound stay in effect.

vkd3d-proton's line for such a signature ends in `fetching vertices`:

```
info:d3d12_command_signature_init_state_template_emulated: Emulating ExecuteIndirect state (4 patch words, slot stride 256, fetching vertices) without EXT_dgc.
```

### Limits of the vertex fetch

- **The scratch buffers have a fixed size.** vkd3d-proton allocates them
  while it records, before the GPU knows how much the sequences draw. One
  `ExecuteIndirect` call can fetch 1024 vertices for each sequence of
  `MaxCommandCount`, at least 16,384 and at most 1,048,576, the same number
  of instance elements, and four times as many indices. The environment
  variable `VKD3D_EXECUTE_INDIRECT_VERTEX_BUDGET` sets the number of vertices
  instead. A draw that does not fit is not drawn. The GPU records that, and
  vkd3d-proton logs an error when the command allocator is reset or
  released:
  `err:d3d12_command_allocator_report_execute_indirect_dropped: 1 emulated ExecuteIndirect call(s) dropped draws that did not fit the vertex scratch buffers.`
- **Cost.** Every call copies the vertices and indices it draws, with two to
  four compute dispatches. The two layout passes loop over `MaxCommandCount`
  in one invocation. Scratch buffers above 1 MiB do not come from
  vkd3d-proton's scratch pool: it creates them for the call and logs
  `Requesting scratch buffer kind 0 larger than limit`. No frame rate was
  measured.
- **Strip cut.** The cut value follows the index format (`0xFFFF` for 16-bit,
  `0xFFFFFFFF` for 32-bit), as in vkd3d-proton's ordinary indexed draws.
- **The fetch reads the argument, index and vertex buffers in a compute
  shader, by address.** It relies on the barriers the application's own
  resource transitions record, as the `0006` patch pass does for the argument
  buffer.

### The check

`ei_views_probe.exe` (source `probes/d3d12-ei/ei_views_probe.cpp`, built by
`probes/d3d12-ei/build.sh`) draws a 5×5 grid of cells with five
`ExecuteIndirect` calls and checks every cell:

| Call | Signature | What it covers |
| --- | --- | --- |
| A | vertex buffer view (slot 0), index buffer view, root constants, indexed draw | Eight sequences, seven by the count buffer. 16-bit and 32-bit indices, `StartIndexLocation`, positive and negative `BaseVertexLocation`, two vertex strides. Slot 1 is bound on the CPU. |
| B | vertex buffer view (slot 1, per instance), root constants, draw | Two instances a sequence, `StartInstanceLocation`. Slot 0 is bound on the CPU and read from `StartVertexLocation`. |
| C | vertex buffer view (slot 0), index buffer view, indexed draw | Triangle strips with the `0xFFFF` strip cut, two quads a sequence. No root argument. |
| D | vertex buffer view (slot 0), draw | Not indexed, `StartVertexLocation`. |
| E | index buffer view, indexed draw | Only the index buffer comes from the arguments. |

The sequences take their buffers from two resources, one in an upload heap
and one in a default heap, at different offsets. Before each call the probe
binds buffers that draw nothing and root constants that draw black, which the
signature must replace. The vertex shader draws magenta when `SV_VertexID` or
`SV_InstanceID` is not what Direct3D defines. The argument and count buffers
are in a default heap and are written by a copy in the same command list, so
the arguments exist only on the GPU when `ExecuteIndirect` is recorded.

The probe draws the scene on the default adapter and, in a child process, on
WARP. The child runs from a temporary directory with Microsoft's `d3d12.dll`
(the package keeps it as `System32\vmlab-win3d\d3d12-native.dll`) and
`D3D12Core.dll`, as the repro for `0006` did. Both pictures are compared with
the expected colour of every cell and with each other, with 1/255 allowed for
each channel. The probe also counts the `fetching vertices` lines in
vkd3d-proton's log of its run.

`tests/e2e/ei.sh <lab-dir> <vm> <evidence-dir>` runs the x64 and the x86
probe in a lab where the package is installed. A run passes when the probe
passed, the default adapter is not a software renderer, and vkd3d-proton
logged all five signatures as emulated.

### Result of the check

Fresh `windows-11` labs (template `26100.1742.11`) on helios with
`hypervisor = "vmlab"`: Nvidia2 is an RTX 5090 (`0000:05:00.0`), AMD1 is RADV
on the AMD iGPU (`0000:72:00.0`). No Xid was logged.

| Check | Nvidia2 | AMD1 |
| --- | --- | --- |
| `tests/e2e/ei.sh`, x64 | pass: 0 of 25 cells wrong, 0 of 102,400 pixels differ from WARP, 5 of 5 signatures emulated | pass: the same numbers |
| `tests/e2e/ei.sh`, x86 | pass: the same numbers | pass: the same numbers |
| `tests/apps/d3d12/run.sh`, `D3D12ExecuteIndirect` | pass: exit 0, no app-local module, the capture shows the triangles | pass: the same |

WARP itself drew all 25 cells as expected in every run.

How the labs got the patch: `tests/e2e/run.sh --keep` installed the package
built at `1ef33863`, and the checks passed with it on Nvidia2. The patch then
got one more change (a sequence that draws nothing clears its slot). The
numbers above are from the `vkd3dcore.dll` files of the build with that
change, copied over the installed ones, with their hashes compared in the
guest. On AMD1 the install was interrupted when the test rig stopped the VM,
and it finished at the next start.

Two controls ran on Nvidia2, in the same lab:

- **Without `0007`.** With the `vkd3dcore.dll` of a build that has patches
  `0001` to `0006` only, the probe fails: 19 of 25 cells are wrong, 19,456
  pixels differ from WARP, and no signature is logged as emulated. The
  picture is empty: the 19 wrong cells are the 19 that have a quad.
- **A budget that is too small.** With
  `VKD3D_EXECUTE_INDIRECT_VERTEX_BUDGET=8`, the probe fails with 8 cells
  wrong, which are the draws that do not fit 8 vertices, and vkd3d-proton
  logs `3 emulated ExecuteIndirect call(s) dropped draws`.

Not checked: a frame rate, a real application that uses such signatures (none
is known here), and a call large enough to leave vkd3d-proton's scratch pool.

The evidence (probe results, both pictures of each run, vkd3d-proton's logs,
the sample's captures) is on helios in
`/home/vmlab/src/stc/evidence/card-254/`.

## What remains

- **Vertex and index buffer view arguments** were dropped by `0006`.
  `0007` emulates them, with the limits listed
  [below](#limits-of-the-vertex-fetch).
- **Root CBVs through push descriptors.** On NVIDIA, vkd3d-proton normally
  binds root CBVs as push descriptors, which it measured as faster. Without
  DGC, they are now raw VAs on every vendor.
- **The push UBO for every root signature** costs a 256-byte upload and a
  push-descriptor update per draw that changes root state, instead of a push
  constant update. The four other samples, the D3D12 probe and the e2e runs
  pass with it. No frame rate was measured.
- **Cost per sequence.** Each sequence records one push-descriptor update
  and one indirect command, so a signature with `MaxCommandCount` in the
  tens of thousands records that many commands. The sample uses 1024.
- **Ray tracing** signatures (`DispatchRays` with state) stay unimplemented,
  as with DGC.
