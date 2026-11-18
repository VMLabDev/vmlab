// RISCV Sail Model
//
// This Sail RISC-V architecture model, comprising all files and
// directories except for the third party dependencies in the
// 'dependencies' directory (which include copies of their licences), is
// subject to the BSD two-clause licence below.
//
// Copyright (c) 2017-2025
//    ahadali5000
//    Alasdair Armstrong
//    Alexander Richardson
//    Aril Computer Corp., for contributions by Scott Johnson
//    Ben Marshall
//    Bicheng Yang
//    Bilal Sakhawat
//    Brian Campbell
//    Chris Casinghino
//    Christopher Pulte
//    Codasip, for contributions by Tim Hutt, Martin Berger and Ben Fletcher
//    dylux
//    eroom1966
//    Google LLC, for contributions by its employees
//    Hesham Almatary
//    Jan Henrik Weinstock
//    Jessica Clarke
//    Jon French
//    Jordan Carlin
//    Martin Berger
//    Michael Sammler
//    Microsoft, for contributions by Robert Norton-Wright and Nathaniel Wesley Filardo
//    Muhammad Bilal Sakhawat
//    Nathaniel Wesley Filardo
//    Paul A. Clarke
//    Peter Rugg
//    Peter Sewell
//    Philipp Tomsich
//    Prashanth Mundkur
//    Rafael Sene
//    Rishiyur S. Nikhil (Bluespec, Inc.)
//    Robert Norton-Wright
//    Scott Johnson (Aril Inc.)
//    Shaked Flur
//    Thibaut Pérami
//    Thomas Bauereiss
//    VRULL GmbH, for contributions by its employees
//    William McSpaddden
//    Xinlai Wan
//
// For a complete list of authors run this command:
//
//     git shortlog --summary --numbered --email
//
// All rights reserved.
//
// This software was developed by the above within the Rigorous
// Engineering of Mainstream Systems (REMS) project, partly funded by
// EPSRC grant EP/K008528/1, at the Universities of Cambridge and
// Edinburgh.
//
// This software was developed by SRI International and the University of
// Cambridge Computer Laboratory (Department of Computer Science and
// Technology) under DARPA/AFRL contract FA8650-18-C-7809 ("CIFV"), and
// under DARPA contract HR0011-18-C-0016 ("ECATS") as part of the DARPA
// SSITH research programme.
//
// This project has received funding from the European Research Council
// (ERC) under the European Union’s Horizon 2020 research and innovation
// programme (grant agreement 789108, ELVER).
//
// ------------------------------------------------------------------
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in
//    the documentation and/or other materials provided with the
//    distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS''
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
// TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
// PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF
// USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
// ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
// OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.
// Sail and the Sail architecture models here, comprising all files and
// directories except the ASL-derived Sail code in the aarch64 directory,
// the logo in the etc/logo/ directory (by Jean Pichon, under CC0), and
// various exceptions detailed in THIRD_PARTY_FILES.md are subject to the
// BSD two-clause licence below.
//
// The ASL derived parts of the ARMv8.3 specification in
// aarch64/no_vector and aarch64/full are copyright ARM Ltd.
//
// This work was partially supported by EPSRC grant EP/K008528/1
// REMS: Rigorous Engineering for Mainstream Systems, an ARM iCASE award,
// EPSRC IAA KTF funding, and donations from Arm. This project has
// received funding from the European Research Council (ERC) under the
// European Union’s Horizon 2020 research and innovation programme (grant
// agreement No 789108, ELVER).
//
// This software was developed by SRI International and the University of
// Cambridge Computer Laboratory (Department of Computer Science and
// Technology) under DARPA/AFRL contracts FA8650-18-C-7809 ("CIFV")
// and FA8750-10-C-0237 ("CTSRD").
//
// -----
//
// Sail
//
// Copyright (c) 2013-2025
//   Kathyrn Gray
//   Shaked Flur
//   Stephen Kell
//   Gabriel Kerneis
//   Robert Norton-Wright
//   Christopher Pulte
//   Peter Sewell
//   Alasdair Armstrong
//   Brian Campbell
//   Thomas Bauereiss
//   Anthony Fox
//   Jon French
//   Dominic Mulligan
//   Stephen Kell
//   Mark Wassell
//   Alastair Reid (Arm Ltd)
//
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in
//    the documentation and/or other materials provided with the
//    distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS''
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
// TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
// PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF
// USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
// ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
// OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.
// Generated by vmlab-hv-tcg-emit. Do not edit.
// Sail RISC-V model, BSD-2-Clause. See ../LICENSE.sail-riscv.
// Original copyrights and terms are retained in that file and THIRD_PARTY_NOTICES.
pub const IR_SHA256: &str = "78c0fe0b8aef56fa7f41471331cb076254cba8512c20b2bb5d0f446832be7aaa";
pub const ENCODED_IR_BYTES: usize = 21876036;
use std::io::Read;
use std::sync::OnceLock;
use vmlab_hv_tcg_ir::{Function, Program};
pub fn model() -> &'static Program {
    static MODEL: OnceLock<Program> = OnceLock::new();
    MODEL.get_or_init(|| {
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(include_bytes!("model.json.gz").as_slice())
            .read_to_end(&mut json)
            .expect("generated model gzip");
        serde_json::from_slice(&json).expect("generated model IR")
    })
}
pub fn decoder(compressed: bool) -> &'static Function {
    &model().functions[if compressed {
        "zencdec_compressed_backwards"
    } else {
        "zencdec_backwards"
    }]
}
pub const INSTRUCTION_TAGS: &[&str] = &[
    "zADDIW",
    "zAES32DSI",
    "zAES32DSMI",
    "zAES32ESI",
    "zAES32ESMI",
    "zAES64DS",
    "zAES64DSM",
    "zAES64ES",
    "zAES64ESM",
    "zAES64IM",
    "zAES64KS1I",
    "zAES64KS2",
    "zAMO",
    "zBITYPE",
    "zBREV8",
    "zBTYPE",
    "zCLMUL",
    "zCLMULH",
    "zCLMULR",
    "zCLZ",
    "zCLZW",
    "zCPOP",
    "zCPOPW",
    "zCSRImm",
    "zCSRReg",
    "zCTZ",
    "zCTZW",
    "zC_ADD",
    "zC_ADDI",
    "zC_ADDI16SP",
    "zC_ADDI4SPN",
    "zC_ADDIW",
    "zC_ADDW",
    "zC_AND",
    "zC_ANDI",
    "zC_BEQZ",
    "zC_BNEZ",
    "zC_EBREAK",
    "zC_FLD",
    "zC_FLDSP",
    "zC_FLW",
    "zC_FLWSP",
    "zC_FSD",
    "zC_FSDSP",
    "zC_FSW",
    "zC_FSWSP",
    "zC_ILLEGAL",
    "zC_J",
    "zC_JAL",
    "zC_JALR",
    "zC_JR",
    "zC_LBU",
    "zC_LD",
    "zC_LDSP",
    "zC_LH",
    "zC_LHU",
    "zC_LI",
    "zC_LUI",
    "zC_LW",
    "zC_LWSP",
    "zC_MUL",
    "zC_MV",
    "zC_NOP",
    "zC_NOT",
    "zC_NTL",
    "zC_OR",
    "zC_SB",
    "zC_SD",
    "zC_SDSP",
    "zC_SEXT_B",
    "zC_SEXT_H",
    "zC_SH",
    "zC_SLLI",
    "zC_SRAI",
    "zC_SRLI",
    "zC_SSPOPCHK",
    "zC_SSPUSH",
    "zC_SUB",
    "zC_SUBW",
    "zC_SW",
    "zC_SWSP",
    "zC_XOR",
    "zC_ZEXT_B",
    "zC_ZEXT_H",
    "zC_ZEXT_W",
    "zDIV",
    "zDIVW",
    "zEBREAK",
    "zECALL",
    "zFCVTMOD_W_D",
    "zFCVT_BF16_S",
    "zFCVT_S_BF16",
    "zFENCE",
    "zFENCEI",
    "zFENCE_TSO",
    "zFLEQ_D",
    "zFLEQ_H",
    "zFLEQ_S",
    "zFLI_D",
    "zFLI_H",
    "zFLI_S",
    "zFLTQ_D",
    "zFLTQ_H",
    "zFLTQ_S",
    "zFMAXM_D",
    "zFMAXM_H",
    "zFMAXM_S",
    "zFMINM_D",
    "zFMINM_H",
    "zFMINM_S",
    "zFMVH_X_D",
    "zFMVP_D_X",
    "zFROUNDNX_D",
    "zFROUNDNX_H",
    "zFROUNDNX_S",
    "zFROUND_D",
    "zFROUND_H",
    "zFROUND_S",
    "zFVFMATYPE",
    "zFVFMTYPE",
    "zFVFTYPE",
    "zFVVMATYPE",
    "zFVVMTYPE",
    "zFVVTYPE",
    "zFWFTYPE",
    "zFWVFMATYPE",
    "zFWVFTYPE",
    "zFWVTYPE",
    "zFWVVMATYPE",
    "zFWVVTYPE",
    "zF_BIN_F_TYPE_D",
    "zF_BIN_F_TYPE_H",
    "zF_BIN_RM_TYPE_D",
    "zF_BIN_RM_TYPE_H",
    "zF_BIN_RM_TYPE_S",
    "zF_BIN_TYPE_F_S",
    "zF_BIN_TYPE_X_S",
    "zF_BIN_X_TYPE_D",
    "zF_BIN_X_TYPE_H",
    "zF_MADD_TYPE_D",
    "zF_MADD_TYPE_H",
    "zF_MADD_TYPE_S",
    "zF_UN_F_TYPE_D",
    "zF_UN_F_TYPE_H",
    "zF_UN_RM_FF_TYPE_D",
    "zF_UN_RM_FF_TYPE_H",
    "zF_UN_RM_FF_TYPE_S",
    "zF_UN_RM_FX_TYPE_D",
    "zF_UN_RM_FX_TYPE_H",
    "zF_UN_RM_FX_TYPE_S",
    "zF_UN_RM_XF_TYPE_D",
    "zF_UN_RM_XF_TYPE_H",
    "zF_UN_RM_XF_TYPE_S",
    "zF_UN_TYPE_F_S",
    "zF_UN_TYPE_X_S",
    "zF_UN_X_TYPE_D",
    "zF_UN_X_TYPE_H",
    "zHFENCE_GVMA",
    "zHFENCE_VVMA",
    "zHINVAL_GVMA",
    "zHINVAL_VVMA",
    "zHLVTYPE",
    "zHSV",
    "zILLEGAL",
    "zITYPE",
    "zJAL",
    "zJALR",
    "zLOAD",
    "zLOADRES",
    "zLOAD_FP",
    "zLPAD",
    "zMASKTYPEI",
    "zMASKTYPEV",
    "zMASKTYPEX",
    "zMMTYPE",
    "zMOVETYPEI",
    "zMOVETYPEV",
    "zMOVETYPEX",
    "zMRET",
    "zMUL",
    "zMULW",
    "zMVVCOMPRESS",
    "zMVVMATYPE",
    "zMVVTYPE",
    "zMVXMATYPE",
    "zMVXTYPE",
    "zNISTYPE",
    "zNITYPE",
    "zNTL",
    "zNVSTYPE",
    "zNVTYPE",
    "zNXSTYPE",
    "zNXTYPE",
    "zORCB",
    "zPAUSE",
    "zREM",
    "zREMW",
    "zREV8",
    "zRFVVTYPE",
    "zRFWVVTYPE",
    "zRIVVTYPE",
    "zRMVVTYPE",
    "zRORI",
    "zRORIW",
    "zRTYPE",
    "zRTYPEW",
    "zSFENCE_INVAL_IR",
    "zSFENCE_VMA",
    "zSFENCE_W_INVAL",
    "zSHA256SIG0",
    "zSHA256SIG1",
    "zSHA256SUM0",
    "zSHA256SUM1",
    "zSHA512SIG0",
    "zSHA512SIG0H",
    "zSHA512SIG0L",
    "zSHA512SIG1",
    "zSHA512SIG1H",
    "zSHA512SIG1L",
    "zSHA512SUM0",
    "zSHA512SUM0R",
    "zSHA512SUM1",
    "zSHA512SUM1R",
    "zSHIFTIOP",
    "zSHIFTIWOP",
    "zSINVAL_VMA",
    "zSLLIUW",
    "zSM3P0",
    "zSM3P1",
    "zSM4ED",
    "zSM4KS",
    "zSRET",
    "zSSAMOSWAP",
    "zSSPOPCHK",
    "zSSPUSH",
    "zSSRDP",
    "zSTORE",
    "zSTORECON",
    "zSTORE_FP",
    "zUNZIP",
    "zUTYPE",
    "zVAESDF",
    "zVAESDM",
    "zVAESEF",
    "zVAESEM",
    "zVAESKF1_VI",
    "zVAESKF2_VI",
    "zVAESZ_VS",
    "zVANDN_VV",
    "zVANDN_VX",
    "zVBREV8_V",
    "zVBREV_V",
    "zVCLMULH_VV",
    "zVCLMULH_VX",
    "zVCLMUL_VV",
    "zVCLMUL_VX",
    "zVCLZ_V",
    "zVCPOP_M",
    "zVCPOP_V",
    "zVCTZ_V",
    "zVEXTTYPE",
    "zVFIRST_M",
    "zVFMERGE",
    "zVFMV",
    "zVFMVFS",
    "zVFMVSF",
    "zVFNCVTBF16_F_F_W",
    "zVFNUNARY0",
    "zVFUNARY0",
    "zVFUNARY1",
    "zVFWCVTBF16_F_F_V",
    "zVFWMACCBF16_VF",
    "zVFWMACCBF16_VV",
    "zVFWUNARY0",
    "zVGHSH_VV",
    "zVGMUL_VV",
    "zVICMPTYPE",
    "zVID_V",
    "zVIMCTYPE",
    "zVIMSTYPE",
    "zVIMTYPE",
    "zVIOTA_M",
    "zVISG",
    "zVITYPE",
    "zVLRETYPE",
    "zVLSEGFFTYPE",
    "zVLSEGTYPE",
    "zVLSSEGTYPE",
    "zVLXSEGTYPE",
    "zVMSBF_M",
    "zVMSIF_M",
    "zVMSOF_M",
    "zVMTYPE",
    "zVMVRTYPE",
    "zVMVSX",
    "zVMVXS",
    "zVREV8_V",
    "zVROL_VV",
    "zVROL_VX",
    "zVROR_VI",
    "zVROR_VV",
    "zVROR_VX",
    "zVSETIVLI",
    "zVSETVL",
    "zVSETVLI",
    "zVSHA2MS_VV",
    "zVSM3C_VI",
    "zVSM3ME_VV",
    "zVSM4K_VI",
    "zVSRETYPE",
    "zVSSEGTYPE",
    "zVSSSEGTYPE",
    "zVSXSEGTYPE",
    "zVVCMPTYPE",
    "zVVMCTYPE",
    "zVVMSTYPE",
    "zVVMTYPE",
    "zVVTYPE",
    "zVWSLL_VI",
    "zVWSLL_VV",
    "zVWSLL_VX",
    "zVXCMPTYPE",
    "zVXMCTYPE",
    "zVXMSTYPE",
    "zVXMTYPE",
    "zVXSG",
    "zVXTYPE",
    "zWFI",
    "zWMVVTYPE",
    "zWMVXTYPE",
    "zWRS",
    "zWVTYPE",
    "zWVVTYPE",
    "zWVXTYPE",
    "zWXTYPE",
    "zXPERM4",
    "zXPERM8",
    "zZBA_RTYPE",
    "zZBA_RTYPEUW",
    "zZBB_EXTOP",
    "zZBB_RTYPE",
    "zZBB_RTYPEW",
    "zZBKB_PACKW",
    "zZBKB_RTYPE",
    "zZBS_IOP",
    "zZBS_RTYPE",
    "zZCMOP",
    "zZICBOM",
    "zZICBOP",
    "zZICBOZ",
    "zZICOND_RTYPE",
    "zZILX_LOAD",
    "zZIMOP_MOP_R",
    "zZIMOP_MOP_RR",
    "zZIP",
    "zZVABD_VV",
    "zZVABD_VX",
    "zZVKSHA2TYPE",
    "zZVKSM4RTYPE",
    "zZVWABDA_VV",
    "zZVWABDA_VX",
];
