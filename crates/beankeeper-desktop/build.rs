fn main() {
    println!("cargo:rerun-if-env-changed=BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON");
    println!("cargo:rerun-if-env-changed=BEANKEEPER_LICENSE_SERVICE_URL");
}
