use core::cmp::Ordering;

const ALPHABET_SIZE: i32 = 256;
const BUCKET_A_SIZE: i32 = 256;
const BUCKET_B_SIZE: i32 = ALPHABET_SIZE * ALPHABET_SIZE;

fn sort_typeBstar(
    T: &[u8],
    SA: &mut [i32],
    bucket_A: &mut [i32],
    bucket_B: &mut [i32],
    n: i32,
    _openMP: bool,
) -> i32 {
    let mut t: i32;
    let mut c1: i32;

    let mut i = n - 1;
    let mut m = n;
    let mut c0 = T[(n - 1) as usize] as i32;
    while 0 <= i {
        loop {
            c1 = c0;
            bucket_A[c1 as usize] += 1;
            i -= 1;
            if !(0 <= i && {
                c0 = T[i as usize] as i32;
                c0 >= c1
            }) {
                break;
            }
        }
        if 0 <= i {
            bucket_B[(c0 << 8 | c1) as usize] += 1;
            m -= 1;
            SA[m as usize] = i;
            i -= 1;
            c1 = c0;
            while 0 <= i && {
                c0 = T[i as usize] as i32;
                c0 <= c1
            } {
                bucket_B[(c1 << 8 | c0) as usize] += 1;
                i -= 1;
                c1 = c0;
            }
        }
    }
    m = n - m;
    i = 0;
    let mut j = 0;
    for c0 in 0..ALPHABET_SIZE {
        t = i + bucket_A[c0 as usize];
        bucket_A[c0 as usize] = i + j;
        i = t + bucket_B[(c0 << 8 | c0) as usize];
        for c1 in c0 + 1..ALPHABET_SIZE {
            j += bucket_B[(c0 << 8 | c1) as usize];
            bucket_B[(c0 << 8 | c1) as usize] = j;
            i += bucket_B[(c1 << 8 | c0) as usize];
        }
    }

    if 0 < m {
        let PAb = n - m;
        let ISAb = m;
        for i in (0..m - 1).rev() {
            t = SA[(PAb + i) as usize];
            c0 = T[t as usize] as i32;
            c1 = T[(t + 1) as usize] as i32;
            let fresh189 = &mut (bucket_B[(c0 << 8 | c1) as usize]);
            *fresh189 -= 1;
            SA[*fresh189 as usize] = i;
        }
        t = SA[(PAb + (m - 1)) as usize];
        c0 = T[t as usize] as i32;
        c1 = T[(t + 1) as usize] as i32;
        let fresh190 = &mut (bucket_B[(c0 << 8 | c1) as usize]);
        *fresh190 -= 1;
        SA[*fresh190 as usize] = m - 1;
        c0 = ALPHABET_SIZE - 2;
        j = m;
        while 0 < j {
            for c1 in (c0 + 1..ALPHABET_SIZE).rev() {
                i = bucket_B[(c0 << 8 | c1) as usize];
                if 1 < j - i {
                    let Ok([i_to_j, PAb]) = ({
                        let i = i as usize;
                        let j = j as usize;
                        let PAb = PAb as usize;
                        SA.get_disjoint_mut([i..j, PAb..SA.len()])
                    }) else {
                        panic!();
                    };

                    sssort(T, PAb, i_to_j, 2, n);
                }
                j = i;
            }
            c0 -= 1;
        }
        i = m - 1;
        while 0 <= i {
            if 0 <= SA[i as usize] {
                j = i;
                loop {
                    SA[(ISAb + SA[i as usize]) as usize] = i;
                    i -= 1;
                    if !(0 <= i && 0 <= SA[i as usize]) {
                        break;
                    }
                }
                SA[(i + 1) as usize] = i - j;
                if i <= 0 {
                    break;
                }
            }
            j = i;
            loop {
                SA[i as usize] = !SA[i as usize];
                SA[(ISAb + SA[i as usize]) as usize] = j;
                i -= 1;
                if SA[i as usize] >= 0 {
                    break;
                }
            }
            SA[(ISAb + SA[i as usize]) as usize] = j;
            i -= 1;
        }

        i = n - 1;
        j = m;
        c0 = T[(n - 1) as usize] as i32;
        while 0 <= i {
            i -= 1;
            c1 = c0;
            while 0 <= i && {
                c0 = T[i as usize] as i32;
                c0 >= c1
            } {
                i -= 1;
                c1 = c0;
            }
            if 0 <= i {
                t = i;
                i -= 1;
                c1 = c0;
                while 0 <= i && {
                    c0 = T[i as usize] as i32;
                    c0 <= c1
                } {
                    i -= 1;
                    c1 = c0;
                }
                j -= 1;
                SA[SA[(ISAb + j) as usize] as usize] = if t == 0 || 1 < t - i { t } else { !t };
            }
        }
        bucket_B[(((256 - 1) << 8) | (256 - 1)) as usize] = n;
        let mut k = m - 1;
        for c0 in (0..ALPHABET_SIZE - 1).rev() {
            i = bucket_A[(c0 + 1) as usize] - 1;
            for c1 in (c0 + 1..ALPHABET_SIZE).rev() {
                t = i - bucket_B[(c1 << 8 | c0) as usize];
                bucket_B[(c1 << 8 | c0) as usize] = i;
                i = t;
                j = bucket_B[(c0 << 8 | c1) as usize];
                while j <= k {
                    SA[i as usize] = SA[k as usize];
                    i -= 1;
                    k -= 1;
                }
            }
            bucket_B[((c0 << 8) | (c0 + 1)) as usize] = i - bucket_B[(c0 << 8 | c0) as usize] + 1;
            bucket_B[(c0 << 8 | c0) as usize] = i;
        }
    }
    m
}

fn construct_SA(
    T: &[u8],
    SA: &mut [i32],
    bucket_A: &mut [i32],
    bucket_B: &mut [i32],
    n: i32,
    m: i32,
) {
    let mut s: i32;
    let mut c0: i32;
    let mut c2: i32;
    if 0 < m {
        for c1 in (0..ALPHABET_SIZE - 1).rev() {
            let mut k = i32::MIN;
            let i = bucket_B[((c1 << 8) | (c1 + 1)) as usize];
            c2 = -1;
            for j in (i..bucket_A[(c1 + 1) as usize]).rev() {
                s = SA[j as usize];
                if 0 < s {
                    assert_eq!(T[s as usize] as i32, c1);
                    assert!((s + 1) < n && T[s as usize] as i32 <= T[(s + 1) as usize] as i32);
                    assert!(T[(s - 1) as usize] as i32 <= T[s as usize] as i32);
                    SA[j as usize] = !s;
                    s -= 1;
                    c0 = T[s as usize] as i32;
                    if 0 < s && T[(s - 1) as usize] as i32 > c0 {
                        s = !s;
                    }
                    if c0 != c2 {
                        if 0 <= c2 {
                            bucket_B[(c1 << 8 | c2) as usize] = k;
                        }
                        c2 = c0;
                        k = bucket_B[(c1 << 8 | c2) as usize];
                    }
                    assert!(k < j);
                    assert_ne!(k, i32::MIN);
                    SA[k as usize] = s;
                    k -= 1;
                } else {
                    assert!(s == 0 && T[s as usize] as i32 == c1 || s < 0);
                    SA[j as usize] = !s;
                }
            }
        }
    }
    c2 = T[(n - 1) as usize] as i32;

    let mut k = bucket_A[c2 as usize] as usize;
    SA[k] = if (T[(n - 2) as usize] as i32) < c2 {
        !(n - 1)
    } else {
        n - 1
    };
    k += 1;

    for i in 0..n as usize {
        s = SA[i];
        if 0 < s {
            assert!(T[(s - 1) as usize] as i32 >= T[s as usize] as i32);
            s -= 1;
            c0 = T[s as usize] as i32;
            if s == 0 || (T[(s - 1) as usize] as i32) < c0 {
                s = !s;
            }
            if c0 != c2 {
                bucket_A[c2 as usize] = k as i32;
                c2 = c0;
                k = bucket_A[c2 as usize] as usize;
            }
            assert!(i < k);
            SA[k] = s;
            k += 1;
        } else {
            assert!(s < 0);
            SA[i] = !s;
        }
    }
}

pub(super) fn divsufsort(T: &[u8], SA: &mut [i32], openMP: bool) -> i32 {
    assert_eq!(T.len(), SA.len());
    let n = T.len();

    if n == 0 {
        return 0;
    } else if n == 1 {
        SA[0] = 0;
        return 0;
    } else if n == 2 {
        let m = ((T[0] as i32) < T[1] as i32) as i32;
        SA[(m ^ 1) as usize] = 0;
        SA[m as usize] = 1;
        return 0;
    }

    let mut bucket_A = vec![0i32; BUCKET_A_SIZE as usize];
    let mut bucket_B = vec![0i32; BUCKET_B_SIZE as usize];

    let m = sort_typeBstar(T, SA, &mut bucket_A, &mut bucket_B, n as _, openMP);
    construct_SA(T, SA, &mut bucket_A, &mut bucket_B, n as _, m);

    0
}

#[inline]
fn cmp_suffix_from(T: &[u8], mut a: usize, mut b: usize, n: usize) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    // Bytewise lexicographic compare, bounded by n.
    while a < n && b < n {
        match T[a].cmp(&T[b]) {
            Ordering::Equal => {
                a += 1;
                b += 1;
            }
            ne => return ne,
        }
    }
    // One suffix ended: shorter one is smaller.
    (n - a).cmp(&(n - b))
}

fn sssort(T: &[u8], PA: &[i32], slice: &mut [i32], depth: i32, n: i32) {
    let depth = depth as usize;
    let n = n as usize;

    // Sort ranks by comparing suffixes T[PA[r] + depth .. n).
    slice.sort_unstable_by(|&ra, &rb| {
        let pa = PA[ra as usize] as usize + depth;
        let pb = PA[rb as usize] as usize + depth;
        cmp_suffix_from(T, pa, pb, n)
    });

    let mut s = 0usize;
    while s < slice.len() {
        let a_pos = PA[slice[s] as usize] as usize + depth;
        let mut e = s + 1;
        while e < slice.len() {
            let b_pos = PA[slice[e] as usize] as usize + depth;
            if cmp_suffix_from(T, a_pos, b_pos, n) != Ordering::Equal {
                break;
            }
            e += 1;
        }
        for v in slice[(s + 1)..e].iter_mut() {
            if *v >= 0 {
                *v = !*v;
            }
        }
        s = e;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn basic() {
        let input = &[
            254u8, 255, 254, 255, 254, 255, 254, 255, 254, 255, 254, 255, 254, 255, 254, 255, 254,
            255, 254, 255, 254, 37, 0,
        ];
        let mut output = vec![0i32; input.len()];

        divsufsort(input, &mut output, false);
    }
}
