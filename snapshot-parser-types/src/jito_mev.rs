use solana_program::pubkey::Pubkey;

pub struct JitoMevMeta {
    pub vote_account: Pubkey,
    pub mev_commission: u16,
}
