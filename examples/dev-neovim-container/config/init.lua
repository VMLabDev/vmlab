-- Placed into dev's home by scripts/editor-bits.ws, as `dev`, before that
-- account has ever logged on (PRD §19.8). It lives in ~/.config/nvim rather
-- than in the workspace, which is why it survives reboot, `down`/`up` and a
-- restore to a snapshot taken after it landed — and why a per-machine
-- `destroy` re-creates it from this file rather than losing it.

vim.opt.number = true
vim.opt.expandtab = true
vim.opt.shiftwidth = 2
vim.opt.termguicolors = true

-- Plugins arrive as `git clone`s under ~/.local/share/nvim/site/pack/vmlab/
-- start/, which is Neovim's built-in package path — no plugin manager has to
-- exist first.

-- The workspace is at /src and syncs both ways with ./workspace on the host.
vim.cmd('cd /src')

print('vmlab dev machine — workspace at /src')
