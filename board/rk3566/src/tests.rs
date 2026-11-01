//! Host-tests van de board-logica die zonder ijzer te toetsen is: het plan,
//! de pool, de SoC-woorden.

use super::*;
use crate::soc::{self, GMAC1_M1_PINS, hiword, iomux_reg};

#[test]
fn the_slot_plan_validates_and_keeps_clear_of_the_kern() {
    // Zonder DTB: de luide terugval, en die moet valideren.
    let p = slots::plan(4, 0).unwrap();
    assert_eq!(p.app_cores(), crate::slots::CORES - 1);
    for r in p.pool() {
        assert!(r.base >= POOL_BASE, "pool {r:?} under the pool base");
        assert!(!r.overlaps(abi::Region::new(STRUCT_WINDOW.base.0, STRUCT_WINDOW.size)));
        assert!(!r.overlaps(abi::Region::new(STAGE_WINDOW.base.0, STAGE_WINDOW.size)));
    }
}

#[test]
fn the_pool_is_carved_from_the_banks_minus_the_holes() {
    // De gemeten 2 GB-Radxa: /memory 0x20_0000..0x8000_0000, de DTB op
    // ~0x7ce9d000 (05-08), een initrd op 0x0a20_0000.
    let banks = [abi::Region::new(0x20_0000, 0x7fe0_0000)];
    let holes = [
        abi::Region::new(0x7ce9_d000, 0x1_2000),
        abi::Region::new(0x0a20_0000, 0x400),
    ];
    let (pool, from_dtb) = slots::pool(&banks, &holes);
    assert!(from_dtb);
    for r in pool.iter() {
        assert!(r.base >= POOL_BASE);
        assert_eq!(r.base % 0x20_0000, 0);
        for h in holes {
            assert!(!r.overlaps(h), "{r:?} over {h:?}");
        }
    }
    let total: u64 = pool.iter().map(|r| r.size).sum();
    assert!(total > 0x7000_0000, "{total:#x}");
    // Een bank boven de MMIO-grens telt niet mee.
    let high = [abi::Region::new(0x1_0000_0000, 0x1000_0000)];
    let (_, from_dtb) = slots::pool(&high, &[]);
    assert!(!from_dtb);
}

#[test]
fn app_core_affinity_is_aff1() {
    assert_eq!(slots::mpidr(1), 0x100);
    assert_eq!(slots::mpidr(3), 0x300);
    assert_eq!(slots::core_of(0x8100_0200), 2);
}

#[test]
fn hiword_writes_carry_their_mask() {
    assert_eq!(hiword(1, 1, 12), (1 << 12) | (1 << 28));
    assert_eq!(hiword(0, 0x7F, 8), 0x7F << 24);
    // De GRF-waarden uit dwmac-rk.c.
    assert_eq!(
        hiword(0, 0x7F, 8) | hiword(0, 0x7F, 0),
        soc::GRF_RGMII_DELAYS_ZERO
    );
}

#[test]
fn the_cru_words_match_clk_rk3568() {
    // Gates 3, 4, 5, 10 open: alleen maskerbits, waarde 0 (actief-laag).
    assert_eq!(
        soc::gmac_gates_word(),
        (1 << 19) | (1 << 20) | (1 << 21) | (1 << 26)
    );
    // 1000 = /1 (0), 100 = /5 (3), 10 = /50 (2), in [5:4].
    assert_eq!(soc::gmac_speed_word(1000), 0x3 << 20);
    assert_eq!(soc::gmac_speed_word(100), (3 << 4) | (0x3 << 20));
    assert_eq!(soc::gmac_speed_word(10), (2 << 4) | (0x3 << 20));
}

#[test]
fn the_iomux_offsets_follow_pinctrl_rockchip() {
    // RK_PB6 in bank 4 (mdc): groep 1, tweede register, shift 8.
    let (reg, shift) = iomux_reg(4, 8 + 6);
    assert_eq!(reg.0, soc::GRF.0 + 3 * 0x20 + 8 + 4);
    assert_eq!(shift, 8);
    // RK_PD6 in bank 3 (txd2): groep 3, tweede register.
    let (reg, shift) = iomux_reg(3, 24 + 6);
    assert_eq!(reg.0, soc::GRF.0 + 2 * 0x20 + 24 + 4);
    assert_eq!(shift, 8);
    // Bank 0 hangt aan de PMU-GRF.
    assert_eq!(iomux_reg(0, 0).0, soc::PMU_GRF);
    // Zestien pinnen, allemaal functie 3 behalve de PHY-reset.
    assert_eq!(GMAC1_M1_PINS.iter().filter(|p| p.func == 3).count(), 15);
}

#[test]
fn a_dtb_outside_dram_is_refused() {
    assert!(!in_dram(0, 8));
    assert!(!in_dram(0xFE66_0000, 8));
    assert!(in_dram(0x7ce9_d000, 0x2_0000));
    assert!(!in_dram(u64::MAX - 4, 8));
}

#[test]
fn the_pll_rate_follows_the_linux_table() {
    // RK3036_PLL_RATE(816000000, 1, 68, 2, 1, 1, 0) en (1800000000, 1, 75, 1, 1, 1, 0)
    // uit rk3568_pll_rates.
    let con1 = |refdiv: u32, post2: u32| refdiv | (post2 << 6) | (1 << 12);
    assert_eq!(
        soc::pll_hz(68 | (2 << 12), con1(1, 1), 0),
        Some(816_000_000)
    );
    assert_eq!(
        soc::pll_hz(75 | (1 << 12), con1(1, 1), 0),
        Some(1_800_000_000)
    );
    // Een deler nul is geen klok.
    assert_eq!(soc::pll_hz(68, con1(1, 1), 0), None);
    // Wat de TF-A na een SCMI-rate achterliet (03-10, met de lock-bit):
    // 1800 en 816, en de core hangt beide keren aan de APLL.
    assert_eq!(soc::pll_hz(0x104b, 0x1441, 0), Some(1_800_000_000));
    assert_eq!(soc::pll_hz(0x2044, 0x1441, 0), Some(816_000_000));
    assert!(soc::on_apll(0x80c0) && soc::on_apll(0x00c0) && soc::on_apll(0));
    assert!(!soc::on_apll(0x0040), "bit 6 alone is the GPLL");
}
