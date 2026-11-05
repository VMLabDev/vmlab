/* Standalone EFI ROM: OVMF has no native network boot driver by default. */
#undef EFI_DOWNGRADE_UX

/* Do not delay local disk boot during POST. */
#undef ROM_BANNER_TIMEOUT
#define ROM_BANNER_TIMEOUT 0
