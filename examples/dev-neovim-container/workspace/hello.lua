-- Edit this in nvim over the SSH facade and watch it change in ./workspace on
-- the host, and the other way round. The workspace syncs both ways (PRD
-- §19.6); everything else in this lab is a declaration.

local function main()
  print('hello from the vmlab dev container')
end

main()
