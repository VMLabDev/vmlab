# Windows-legacy smoke template

Use the installed `windows-server-2008-r2-core` 7601.0.0 template for the
phase-4a Windows-legacy lifecycle check. The owner selected this replacement
after the Vista prerequisite was verified. Run agent execution, snapshot
rollback without reboot, in-place reset, and guest poweroff through S1.

Vista 6002.0.0 and the original Server 2008 6001 images intentionally have no
guest agent. Their template sources set `agent = false`; waiting for an agent
does not test those images. Report the observed Vista kernel/Winlogon Welcome
screen separately, without calling it agent-ready or a completed desktop.

The selected R2 Core image is at
`~/.local/share/vmlab/templates/x86_64/windows-server-2008-r2-core/7601.0.0/`.
Its metadata records the `windows-legacy` profile, two CPUs, 4 GiB RAM, and
`agent_version = "agent=57fa802"`. Use a fresh overlay, virtio-serial at PCI
00:02.0, standard VGA at 00:04.0, and no NIC, as in the Windows 7 gate.

The failed Vista agent wait is retained as diagnostic evidence. It does not
establish an agent-transport defect: the required guest software is absent.
