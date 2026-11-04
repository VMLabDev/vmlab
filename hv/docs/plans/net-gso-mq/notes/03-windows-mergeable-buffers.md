# Windows netkvm mergeable-buffer measurement

Measured on a fresh clone of the installed Windows 10 standard template
19045.2006.9, without changing driver settings. Both IP families use MTU 1500.

The installed netkvm driver is 100.101.104.28500, dated 2025-07-09, using NDIS
6.83. Neither a configured MergeableBuffers value nor an Ndi parameter default
was present. This does not establish a numeric registry default.

The actual saved virtio PCI transport reports negotiated features
0x01000001384399a3. MRG_RXBUF (bit 15), CTRL_VQ (bit 17) and MQ (bit 22) are set.
The saved device has five virtqueues: two RX/TX pairs and a control queue.
Mergeable buffers are therefore enabled by the effective shipped default on
this installed template and driver. This resolves research question 4 for this
template, not for every virtio-win release.

Get-NetAdapterRss reports Enabled=true and NumberOfReceiveQueues=16. The latter
is the NDIS configuration value, not evidence of 16 negotiated hardware pairs.

The measurement uses a real control-socket snapshot. A read-only qcow2 reader
installs the saved snapshot's view in memory, checks VLHV and VPCI versions,
and decodes the NIC's negotiated features. It does not apply a snapshot to the
template or write to the image. An earlier active-L1 parsing attempt failed and
was corrected before the result above was recorded.

Evidence is retained under /tmp/ng.K2GzID/logs, including
15-windows-netkvm.json and the windows-negotiated-features JSON records. The
initial clone that entered recovery is preserved separately under
/tmp/ng.K2GzID/attempts/windows-recovery-clone. The measurement above was repeated
on the fresh clone.
