# A plain Ed25519 on Python's big integers (RFC 8032 section 5.1), independent of the crate:
# - it checks the RFC 8032 section 7.1 vectors of `RFC` in mod.rs: each public key and signature
#   is derived here from the RFC's secret key (below, as the RFC prints them) and asserted equal
#   to the RFC's bytes;
# - it generates `EDGE`, `TORSION` and `MIXED_KEYS` in mod.rs, printing each case with the
#   verdicts of the cofactorless and cofactored equations;
# - `verdict` is the strict check the crate makes, step for step, which `convert.py` uses to
#   give each published vector its expected reason.
# Run: python3 -I crates/blaze-ed25519/tests/data/edge.py

import hashlib
p = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
def inv(x): return pow(x, p-2, p)
d = -121665 * inv(121666) % p
I = pow(2, (p-1)//4, p)
def H(m): return hashlib.sha512(m).digest()
def recover_x(y, sign):
    if y >= p: return None
    x2 = (y*y-1) * inv(d*y*y+1)
    if x2 == 0:
        if sign: return None
        return 0
    x = pow(x2, (p+3)//8, p)
    if (x*x - x2) % p != 0: x = x*I % p
    if (x*x - x2) % p != 0: return None
    if (x & 1) != sign: x = p - x
    return x
By = 4*inv(5) % p
Bx = recover_x(By, 0)
G = (Bx, By, 1, Bx*By % p)
def add(P,Q):
    A=(P[1]-P[0])*(Q[1]-Q[0])%p; B=(P[1]+P[0])*(Q[1]+Q[0])%p
    C=2*P[3]*Q[3]*d%p; D=2*P[2]*Q[2]%p
    E,F,Gg,Hh=B-A,D-C,D+C,B+A
    return (E*F%p, Gg*Hh%p, F*Gg%p, E*Hh%p)
def mul(s,P):
    Q=(0,1,1,0)
    while s>0:
        if s&1: Q=add(Q,P)
        P=add(P,P); s>>=1
    return Q
def enc(P):
    zi=inv(P[2]); x=P[0]*zi%p; y=P[1]*zi%p
    return int.to_bytes(y | ((x&1)<<255), 32, 'little')
def dec(s):
    y=int.from_bytes(s,'little'); sign=y>>255; y&=(1<<255)-1
    x=recover_x(y,sign)
    if x is None: return None
    return (x,y,1,x*y%p)
def expand(secret):
    h=H(secret); a=int.from_bytes(h[:32],'little'); a&=(1<<254)-8; a|=(1<<254)
    return a, h[32:]
def pub(secret):
    a,_=expand(secret); return enc(mul(a,G))
def sign(secret,msg):
    a,prefix=expand(secret); A=enc(mul(a,G))
    r=int.from_bytes(H(prefix+msg),'little')%L
    R=enc(mul(r,G))
    k=int.from_bytes(H(R+A+msg),'little')%L
    s=(r+k*a)%L
    return R+int.to_bytes(s,32,'little')

O=(0,1,1,0)
def neg(P): return ((-P[0])%p, P[1], P[2], (-P[3])%p)
def eq(P,Q): return enc(P)==enc(Q)
def is_id(P): return enc(P)==enc(O)
def order_small(P): return is_id(mul(8,P))
def kof(Rb,Ab,M): return int.from_bytes(H(Rb+Ab+M),'little')%L
def cofless(Ab,Rb,S,M):
    A=dec(Ab); R=dec(Rb)
    if A is None or R is None: return None
    k=kof(Rb,Ab,M); return eq(mul(S,G), add(R, mul(k,A)))
def cof(Ab,Rb,S,M):
    A=dec(Ab); R=dec(Rb)
    if A is None or R is None: return None
    k=kof(Rb,Ab,M); return eq(mul(8,mul(S,G)), mul(8,add(R, mul(k,A))))

def encoding_why(b):
    # RFC 8032 5.1.3, with the crate's reasons: y >= p, then no x, then x = 0 with the sign bit.
    y=int.from_bytes(b,'little') & ((1<<255)-1)
    if y >= p: return "YNotReduced"
    x2 = (y*y-1) * inv(d*y*y+1) % p
    x = pow(x2, (p+3)//8, p)
    if (x*x - x2) % p != 0: x = x*I % p
    if (x*x - x2) % p != 0: return "NotOnCurve"
    if x == 0 and b[31] >> 7: return "NegativeZero"
    return None

def verdict(Ab, M, sig):
    # The crate's steps 1 to 7 (src/lib.rs), in its order, and the reason of the first that fails.
    if len(sig) != 64: return "Length"
    why = encoding_why(Ab)
    if why: return "KeyEncoding." + why
    A = dec(Ab)
    if order_small(A): return "KeySmallOrder"
    if not is_id(mul(L, A)): return "KeyMixedOrder"
    Rb, S = sig[:32], int.from_bytes(sig[32:], 'little')
    if S >= L: return "SNotBelowOrder"
    why = encoding_why(Rb)
    if why: return "REncoding." + why
    if order_small(dec(Rb)): return "RSmallOrder"
    return "Ok" if cofless(Ab, Rb, S, M) else "Mismatch"

# RFC 8032 section 7.1, as printed there: (name, secret key, public key, message, signature).
RFC = [
    ("TEST 1",
     "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
     "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
     "",
     "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
    ("TEST 2",
     "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
     "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
     "72",
     "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
    ("TEST 3",
     "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
     "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
     "af82",
     "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a"),
    ("TEST 1024",
     "f5e5767cf153319517630f226876b86c8160cc583bc013744c6bf255f5cc0ee5",
     "278117fc144c72340f67d0f2316e8386ceffbf2b2428c9c51fef7c597f1d426e",
     (
        "08b8b2b733424243760fe426a4b54908632110a66c2f6591eabd3345e3e4eb98fa6e264bf09efe12ee50f8f54e9f77b1"
        "e355f6c50544e23fb1433ddf73be84d879de7c0046dc4996d9e773f4bc9efe5738829adb26c81b37c93a1b270b20329d"
        "658675fc6ea534e0810a4432826bf58c941efb65d57a338bbd2e26640f89ffbc1a858efcb8550ee3a5e1998bd177e93a"
        "7363c344fe6b199ee5d02e82d522c4feba15452f80288a821a579116ec6dad2b3b310da903401aa62100ab5d1a36553e"
        "06203b33890cc9b832f79ef80560ccb9a39ce767967ed628c6ad573cb116dbefefd75499da96bd68a8a97b928a8bbc10"
        "3b6621fcde2beca1231d206be6cd9ec7aff6f6c94fcd7204ed3455c68c83f4a41da4af2b74ef5c53f1d8ac70bdcb7ed1"
        "85ce81bd84359d44254d95629e9855a94a7c1958d1f8ada5d0532ed8a5aa3fb2d17ba70eb6248e594e1a2297acbbb39d"
        "502f1a8c6eb6f1ce22b3de1a1f40cc24554119a831a9aad6079cad88425de6bde1a9187ebb6092cf67bf2b13fd65f270"
        "88d78b7e883c8759d2c4f5c65adb7553878ad575f9fad878e80a0c9ba63bcbcc2732e69485bbc9c90bfbd62481d9089b"
        "eccf80cfe2df16a2cf65bd92dd597b0707e0917af48bbb75fed413d238f5555a7a569d80c3414a8d0859dc65a46128ba"
        "b27af87a71314f318c782b23ebfe808b82b0ce26401d2e22f04d83d1255dc51addd3b75a2b1ae0784504df543af8969b"
        "e3ea7082ff7fc9888c144da2af58429ec96031dbcad3dad9af0dcbaaaf268cb8fcffead94f3c7ca495e056a9b47acdb7"
        "51fb73e666c6c655ade8297297d07ad1ba5e43f1bca32301651339e22904cc8c42f58c30c04aafdb038dda0847dd988d"
        "cda6f3bfd15c4b4c4525004aa06eeff8ca61783aacec57fb3d1f92b0fe2fd1a85f6724517b65e614ad6808d6f6ee34df"
        "f7310fdc82aebfd904b01e1dc54b2927094b2db68d6f903b68401adebf5a7e08d78ff4ef5d63653a65040cf9bfd4aca7"
        "984a74d37145986780fc0b16ac451649de6188a7dbdf191f64b5fc5e2ab47b57f7f7276cd419c17a3ca8e1b939ae49e4"
        "88acba6b965610b5480109c8b17b80e1b7b750dfc7598d5d5011fd2dcc5600a32ef5b52a1ecc820e308aa342721aac09"
        "43bf6686b64b2579376504ccc493d97e6aed3fb0f9cd71a43dd497f01f17c0e2cb3797aa2a2f256656168e6c496afc5f"
        "b93246f6b1116398a346f1a641f3b041e989f7914f90cc2c7fff357876e506b50d334ba77c225bc307ba537152f3f161"
        "0e4eafe595f6d9d90d11faa933a15ef1369546868a7f3a45a96768d40fd9d03412c091c6315cf4fde7cb68606937380d"
        "b2eaaa707b4c4185c32eddcdd306705e4dc1ffc872eeee475a64dfac86aba41c0618983f8741c5ef68d3a101e8a3b8ca"
        "c60c905c15fc910840b94c00a0b9d0"
    ),
     "0aab4c900501b3e24d7cdf4663326a3a87df5e4843b2cbdb67cbf6e460fec350aa5371b1508f9f4528ecea23c436d94b5e8fcd4f681e30a6ac00a9704a188a03"),
    ("TEST SHA(abc)",
     "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
     "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
     (
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd"
        "454d4423643ce80e2a9ac94fa54ca49f"
    ),
     "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704"),
]

def main():
    for name, secret, public, message, signature in RFC:
        secret, public, message, signature = (bytes.fromhex(x) for x in (secret, public, message, signature))
        assert pub(secret) == public, name
        assert sign(secret, message) == signature, name
        assert verdict(public, message, signature) == "Ok", name
        print("RFC", name, "public key and signature derived from the secret key: equal")
    # torsion: [L]P for P = decode of y=2.. until order 8
    y=2
    while True:
        P=dec(int.to_bytes(y,32,'little'))
        if P is not None:
            T=mul(L,P)
            if not is_id(mul(4,T)): break
        y+=1
    T8=T
    tors=[mul(i,T8) for i in range(8)]
    print("TORSION", [enc(t).hex() for t in tors])
    a=int.from_bytes(H(b"blaze-ed25519 edge key"),'little')%L
    r=int.from_bytes(H(b"blaze-ed25519 edge nonce"),'little')%L
    Ap=mul(a,G); Apb=enc(Ap)
    Am=add(Ap,T8); Amb=enc(Am)
    # PRIME_KEY + TORSION[i]: keys of order 8L, 4L, 8L, 2L, 8L, 4L, 8L, each to be refused.
    mixed=[enc(add(Ap,tors[i])) for i in range(1,8)]
    for i,m in enumerate(mixed, 1): assert verdict(m, b"", bytes(64)) == "KeyMixedOrder", i
    print("MIXED_KEYS", [m.hex() for m in mixed])
    cases=[]
    def case(name, Ab, Rb, S, M, why):
        cases.append((name,Ab,Rb,S,M,why,cofless(Ab,Rb,S,M),cof(Ab,Rb,S,M)))
    def search(label, f):
        i=0
        while True:
            M=("%s %d"%(label,i)).encode()
            out=f(M)
            if out: return out
            i+=1
    # C0: small A, small R, S=0, cofactorless holds
    def c0(M):
        for At in tors[1:]:
            for Rt in tors:
                Ab=enc(At); Rb=enc(Rt)
                if cofless(Ab,Rb,0,M): return (Ab,Rb,0,M)
    case("small_key_small_r_s_zero", *search("c0",c0), "KeySmallOrder")
    # C1: small A, mixed R, S=r: R=[r]B+T', need T'=-[k]A
    def c1(M):
        At=tors[1]; Ab=enc(At)
        for Rt in tors[1:]:
            Rb=enc(add(mul(r,G),Rt))
            if cofless(Ab,Rb,r,M): return (Ab,Rb,r,M)
    case("small_key_mixed_r", *search("c1",c1), "KeySmallOrder")
    # C2: mixed A, small R: R=-[k]T8, S=k a
    def c2(M):
        for Rt in tors[1:]:
            Rb=enc(Rt); k=kof(Rb,Amb,M); S=k*a%L
            if cofless(Amb,Rb,S,M): return (Amb,Rb,S,M)
    case("mixed_key_small_r", *search("c2",c2), "KeyMixedOrder")
    # C3: mixed A, mixed R, cofactorless holds
    def c3(M):
        for Rt in tors[1:]:
            Rb=enc(add(mul(r,G),Rt)); k=kof(Rb,Amb,M); S=(r+k*a)%L
            if cofless(Amb,Rb,S,M): return (Amb,Rb,S,M)
    case("mixed_key_mixed_r_both_hold", *search("c3",c3), "KeyMixedOrder")
    # C4: mixed A, mixed R, cofactored holds, cofactorless fails
    def c4(M):
        Rb=enc(add(mul(r,G),tors[1])); k=kof(Rb,Amb,M); S=(r+k*a)%L
        if not cofless(Amb,Rb,S,M) and cof(Amb,Rb,S,M): return (Amb,Rb,S,M)
    case("mixed_key_mixed_r_cofactored_only", *search("c4",c4), "KeyMixedOrder")
    # C5: prime A, mixed R: cofactored holds, cofactorless fails
    M=b"c5"; Rb=enc(add(mul(r,G),tors[1])); k=kof(Rb,Apb,M); S=(r+k*a)%L
    case("prime_key_mixed_r", Apb,Rb,S,M, "Mismatch")
    # valid, and two second forms of its S
    M=b"valid"; Rb=enc(mul(r,G)); k=kof(Rb,Apb,M); S=(r+k*a)%L
    case("valid", Apb,Rb,S,M, "Accept")
    case("s_plus_order", Apb,Rb,S+L,M, "SNotBelowOrder")
    case("s_high_bits", Apb,Rb,S+((2**256-1-S)//L)*L,M, "SNotBelowOrder")
    # R = identity, prime A: S = k a
    M=b"identity r"; Rb=enc(O); k=kof(Rb,Apb,M); S=k*a%L
    case("identity_r", Apb,Rb,S,M, "RSmallOrder")
    # non-canonical R: identity as y = p+1, S = k a with k over those bytes
    Rnc=int.to_bytes(p+1,32,'little'); k=kof(Rnc,Apb,M); S=k*a%L
    case("noncanonical_identity_r", Apb,Rnc,S,M, "RNotReduced")
    # non-canonical A (p+1 = identity), S=0, R=identity, k*O=O -> equation holds
    Anc=int.to_bytes(p+1,32,'little')
    case("noncanonical_identity_key", Anc,enc(O),0,b"nc key", "KeyNotReduced")
    # x=0 with the sign bit: the identity "negative"; S = k a with k over these bytes, so a
    # decoder that took the sign bit leniently would see the equation hold
    negzero=bytes([1]+[0]*30+[0x80])
    k=kof(negzero,Apb,M); S=k*a%L
    assert eq(mul(S,G), add(O, mul(k,Ap)))
    case("negative_zero_r", Apb,negzero,S,M, "RNegativeZero")
    case("negative_zero_key", negzero,enc(O),0,b"nz key", "KeyNegativeZero")
    for c in cases:
        name,Ab,Rb,S,M,why,cl,cf=c
        Sb=int.to_bytes(S,32,'little')
        print("CASE",name,Ab.hex(),(Rb+Sb).hex(),M.hex(),why,"cofactorless=%s cofactored=%s"%(cl,cf))
    print("APRIME",Apb.hex(),"AMIXED",Amb.hex())

if __name__ == "__main__":
    main()
