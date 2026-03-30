use sysinfo::Disks;

pub fn get_disks() -> Disks {
    Disks::new_with_refreshed_list()
}
