/// Simple bitflags macro to avoid pulling in the bitflags crate
macro_rules! bitflags {
    (
        $(#[$outer:meta])*
        $vis:vis struct $Name:ident: $T:ty {
            $(
                $(#[$inner:meta])*
                const $Flag:ident = $value:expr;
            )*
        }
    ) => {
        $(#[$outer])*
        $vis struct $Name {
            bits: $T,
        }

        impl $Name {
            $(
                $(#[$inner])*
                pub const $Flag: Self = Self { bits: $value };
            )*

            pub const fn empty() -> Self {
                Self { bits: 0 }
            }

            pub const fn bits(&self) -> $T {
                self.bits
            }

            pub const fn from_bits_truncate(bits: $T) -> Self {
                Self { bits }
            }

            pub const fn contains(&self, other: Self) -> bool {
                (self.bits & other.bits) == other.bits
            }
        }

        impl std::ops::BitOr for $Name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                Self { bits: self.bits | rhs.bits }
            }
        }

        impl std::ops::BitOrAssign for $Name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.bits |= rhs.bits;
            }
        }
    };
}
