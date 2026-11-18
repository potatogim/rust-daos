// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// ABI probe: compiled against the real DAOS headers of one tag, it prints one
// line per layout fact that the crate declares on its own. The Rust unit test
// `sys::tests::abi_layout_matches_probe_files` prints the same lines from the
// crate's declarations and compares them with the files this program writes.
//
// Line format (space separated):
//   sizeof <type> <bytes>
//   offsetof <type> <field> <bytes>
//   width <type> <bits> <signed:0|1>
//   const <name> <value>
//   der <name> <value>

#include <stddef.h>
#include <stdio.h>
#include <sys/stat.h>

#include <gurt/types.h>
#include <daos_errno.h>
#include <daos_types.h>
#include <daos_obj_class.h>
#include <daos_obj.h>
#include <daos_prop.h>
#include <daos_pool.h>
#include <daos_cont.h>
#include <daos_kv.h>
#include <daos_array.h>
#include <daos_api.h>
#include <daos_fs.h>

/*
 * Deliberately not probed: constants whose values differ between DAOS lines
 * and that the crate therefore does not carry (DAOS_PROP_PERF_DOMAIN_GROUP,
 * DAOS_PROP_PO_REDUN_FAC_DEFAULT, DAOS_COO_NBITS, the *P3 object classes,
 * DAOS_PROP_CO_REDUN_FAULT), and fields that changed name at the same offset
 * (daos_target_info_t ta_type/ta_padding, daos_rebuild_status bytes 16 to
 * 19): their neighbours and the struct size are probed instead.
 */

#define SIZEOF(t) printf("sizeof %s %zu\n", #t, sizeof(t))
#define OFFSETOF(t, f) printf("offsetof %s %s %zu\n", #t, #f, offsetof(t, f))
#define WIDTH(t) printf("width %s %zu %d\n", #t, sizeof(t) * 8, ((t)-1) < (t)0 ? 1 : 0)
#define CONST(c) printf("const %s %lld\n", #c, (long long)(c))
#define DER(name, desc) printf("der %s %d\n", #name, (int)(name));
#define SIZEOF_AS(t, label) printf("sizeof %s %zu\n", label, sizeof(t))
#define OFFSETOF_AS(t, label, f) printf("offsetof %s %s %zu\n", label, #f, offsetof(t, f))
#define WIDTH_AS(t, label) printf("width %s %zu %d\n", label, sizeof(t) * 8, ((t)-1) < (t)0 ? 1 : 0)
#define OBJ_TYPE(t) printf("const %s %d\n", #t, (int)(t))

int
main(void)
{
	SIZEOF(d_iov_t);
	OFFSETOF(d_iov_t, iov_buf);
	OFFSETOF(d_iov_t, iov_buf_len);
	OFFSETOF(d_iov_t, iov_len);

	SIZEOF(d_sg_list_t);
	OFFSETOF(d_sg_list_t, sg_nr);
	OFFSETOF(d_sg_list_t, sg_nr_out);
	OFFSETOF(d_sg_list_t, sg_iovs);

	SIZEOF(daos_anchor_t);
	OFFSETOF(daos_anchor_t, da_type);
	OFFSETOF(daos_anchor_t, da_shard);
	OFFSETOF(daos_anchor_t, da_flags);
	OFFSETOF(daos_anchor_t, da_sub_anchors);
	OFFSETOF(daos_anchor_t, da_buf);

	SIZEOF(daos_obj_id_t);
	OFFSETOF(daos_obj_id_t, lo);
	OFFSETOF(daos_obj_id_t, hi);

	WIDTH(daos_size_t);
	WIDTH(daos_off_t);
	WIDTH(daos_oclass_id_t);
	WIDTH(mode_t);

	CONST(DAOS_ANCHOR_TYPE_EOF);
	CONST(DAOS_ANCHOR_BUF_MAX);
	CONST(DFS_MAX_NAME);
	CONST(DFS_MAX_PATH);
	CONST(OC_UNKNOWN);

	/* Pool, container, object, array and transaction declarations. */
	SIZEOF_AS(daos_handle_t, "daos_handle_t");
	OFFSETOF_AS(daos_handle_t, "daos_handle_t", cookie);
	SIZEOF_AS(daos_epoch_range_t, "daos_epoch_range_t");
	OFFSETOF_AS(daos_epoch_range_t, "daos_epoch_range_t", epr_lo);
	OFFSETOF_AS(daos_epoch_range_t, "daos_epoch_range_t", epr_hi);
	SIZEOF_AS(struct daos_space, "daos_space");
	OFFSETOF_AS(struct daos_space, "daos_space", s_total);
	OFFSETOF_AS(struct daos_space, "daos_space", s_free);
	SIZEOF_AS(struct daos_pool_space, "daos_pool_space");
	OFFSETOF_AS(struct daos_pool_space, "daos_pool_space", ps_space);
	OFFSETOF_AS(struct daos_pool_space, "daos_pool_space", ps_free_min);
	OFFSETOF_AS(struct daos_pool_space, "daos_pool_space", ps_free_max);
	OFFSETOF_AS(struct daos_pool_space, "daos_pool_space", ps_free_mean);
	OFFSETOF_AS(struct daos_pool_space, "daos_pool_space", ps_ntargets);
	SIZEOF_AS(struct daos_rebuild_status, "daos_rebuild_status");
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_version);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_seconds);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_errno);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_state);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_fail_rank);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_toberb_obj_nr);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_obj_nr);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_rec_nr);
	OFFSETOF_AS(struct daos_rebuild_status, "daos_rebuild_status", rs_size);
	SIZEOF_AS(daos_pool_info_t, "daos_pool_info_t");
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_uuid);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_ntargets);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_nnodes);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_ndisabled);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_map_ver);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_leader);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_bits);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_space);
	OFFSETOF_AS(daos_pool_info_t, "daos_pool_info_t", pi_rebuild_st);
	SIZEOF_AS(daos_target_info_t, "daos_target_info_t");
	OFFSETOF_AS(daos_target_info_t, "daos_target_info_t", ta_state);
	OFFSETOF_AS(daos_target_info_t, "daos_target_info_t", ta_perf);
	OFFSETOF_AS(daos_target_info_t, "daos_target_info_t", ta_space);
	SIZEOF_AS(struct daos_pool_cont_info, "daos_pool_cont_info");
	OFFSETOF_AS(struct daos_pool_cont_info, "daos_pool_cont_info", pci_uuid);
	OFFSETOF_AS(struct daos_pool_cont_info, "daos_pool_cont_info", pci_label);
	SIZEOF_AS(daos_cont_info_t, "daos_cont_info_t");
	OFFSETOF_AS(daos_cont_info_t, "daos_cont_info_t", ci_uuid);
	OFFSETOF_AS(daos_cont_info_t, "daos_cont_info_t", ci_lsnapshot);
	OFFSETOF_AS(daos_cont_info_t, "daos_cont_info_t", ci_nhandles);
	OFFSETOF_AS(daos_cont_info_t, "daos_cont_info_t", ci_nsnapshots);
	OFFSETOF_AS(daos_cont_info_t, "daos_cont_info_t", ci_md_otime);
	OFFSETOF_AS(daos_cont_info_t, "daos_cont_info_t", ci_md_mtime);
	SIZEOF_AS(struct daos_prop_entry, "daos_prop_entry");
	OFFSETOF_AS(struct daos_prop_entry, "daos_prop_entry", dpe_type);
	OFFSETOF_AS(struct daos_prop_entry, "daos_prop_entry", dpe_flags);
	OFFSETOF_AS(struct daos_prop_entry, "daos_prop_entry", dpe_reserv);
	OFFSETOF_AS(struct daos_prop_entry, "daos_prop_entry", dpe_val);
	SIZEOF_AS(daos_prop_t, "daos_prop_t");
	OFFSETOF_AS(daos_prop_t, "daos_prop_t", dpp_nr);
	OFFSETOF_AS(daos_prop_t, "daos_prop_t", dpp_reserv);
	OFFSETOF_AS(daos_prop_t, "daos_prop_t", dpp_entries);
	SIZEOF_AS(dfs_attr_t, "dfs_attr_t");
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_id);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_chunk_size);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_oclass_id);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_props);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_mode);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_dir_oclass_id);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_file_oclass_id);
	OFFSETOF_AS(dfs_attr_t, "dfs_attr_t", da_hints);
	SIZEOF_AS(daos_recx_t, "daos_recx_t");
	OFFSETOF_AS(daos_recx_t, "daos_recx_t", rx_idx);
	OFFSETOF_AS(daos_recx_t, "daos_recx_t", rx_nr);
	SIZEOF_AS(daos_iod_t, "daos_iod_t");
	OFFSETOF_AS(daos_iod_t, "daos_iod_t", iod_name);
	OFFSETOF_AS(daos_iod_t, "daos_iod_t", iod_type);
	OFFSETOF_AS(daos_iod_t, "daos_iod_t", iod_size);
	OFFSETOF_AS(daos_iod_t, "daos_iod_t", iod_flags);
	OFFSETOF_AS(daos_iod_t, "daos_iod_t", iod_nr);
	OFFSETOF_AS(daos_iod_t, "daos_iod_t", iod_recxs);
	SIZEOF_AS(daos_key_desc_t, "daos_key_desc_t");
	OFFSETOF_AS(daos_key_desc_t, "daos_key_desc_t", kd_key_len);
	OFFSETOF_AS(daos_key_desc_t, "daos_key_desc_t", kd_val_type);
	SIZEOF_AS(daos_range_t, "daos_range_t");
	OFFSETOF_AS(daos_range_t, "daos_range_t", rg_idx);
	OFFSETOF_AS(daos_range_t, "daos_range_t", rg_len);
	SIZEOF_AS(daos_array_iod_t, "daos_array_iod_t");
	OFFSETOF_AS(daos_array_iod_t, "daos_array_iod_t", arr_nr);
	OFFSETOF_AS(daos_array_iod_t, "daos_array_iod_t", arr_rgs);
	OFFSETOF_AS(daos_array_iod_t, "daos_array_iod_t", arr_nr_short_read);
	OFFSETOF_AS(daos_array_iod_t, "daos_array_iod_t", arr_nr_read);
	SIZEOF_AS(daos_array_stbuf_t, "daos_array_stbuf_t");
	OFFSETOF_AS(daos_array_stbuf_t, "daos_array_stbuf_t", st_size);
	OFFSETOF_AS(daos_array_stbuf_t, "daos_array_stbuf_t", st_max_epoch);
	WIDTH_AS(daos_epoch_t, "daos_epoch_t");
	WIDTH_AS(d_rank_t, "d_rank_t");
	WIDTH_AS(daos_iod_type_t, "daos_iod_type_t");
	WIDTH_AS(daos_oclass_hints_t, "daos_oclass_hints_t");
	WIDTH_AS(enum daos_otype_t, "daos_otype_t");
	CONST(DAOS_PC_RO);
	CONST(DAOS_PC_RW);
	CONST(DAOS_COO_RO);
	CONST(DAOS_COO_RW);
	CONST(DAOS_COO_FORCE);
	CONST(DPI_SPACE);
	CONST(DPI_REBUILD_STATUS);
	CONST(DAOS_PROP_LABEL_MAX_LEN);
	CONST(DAOS_PROP_ENTRIES_MAX_NR);
	CONST(DAOS_CONT_HINT_MAX_LEN);
	CONST(DAOS_PROP_CO_LABEL);
	CONST(DAOS_PROP_CO_LAYOUT_TYPE);
	CONST(DAOS_PROP_CO_LAYOUT_VER);
	CONST(DAOS_PROP_CO_CSUM);
	CONST(DAOS_PROP_CO_REDUN_FAC);
	CONST(DAOS_PROP_CO_REDUN_LVL);
	CONST(DAOS_PROP_CO_SNAPSHOT_MAX);
	CONST(DAOS_PROP_CO_OWNER);
	CONST(DAOS_PROP_CO_OWNER_GROUP);
	CONST(DAOS_PROP_CO_STATUS);
	CONST(DAOS_PROP_CO_ALLOCED_OID);
	CONST(DAOS_PROP_CO_EC_CELL_SZ);
	CONST(DAOS_PROP_ENTRY_NOT_SET);
	CONST(DAOS_PROP_CO_LAYOUT_UNKNOWN);
	CONST(DAOS_PROP_CO_LAYOUT_POSIX);
	CONST(DAOS_PROP_CO_LAYOUT_HDF5);
	CONST(DAOS_PROP_CO_LAYOUT_PYTHON);
	CONST(DAOS_PROP_CO_REDUN_RANK);
	CONST(DAOS_PROP_CO_REDUN_NODE);
	CONST(DAOS_IOD_SINGLE);
	CONST(DAOS_IOD_ARRAY);
	CONST(DAOS_REC_ANY);
	CONST(DAOS_OO_RO);
	CONST(DAOS_OO_RW);
	CONST(DAOS_OO_EXCL);
	CONST(DAOS_COND_PUNCH);
	CONST(DAOS_COND_DKEY_INSERT);
	CONST(DAOS_COND_DKEY_UPDATE);
	CONST(DAOS_COND_DKEY_FETCH);
	CONST(DAOS_COND_AKEY_INSERT);
	CONST(DAOS_COND_AKEY_UPDATE);
	CONST(DAOS_COND_AKEY_FETCH);
	CONST(DAOS_COND_KEY_INSERT);
	CONST(DAOS_GET_MAX);
	CONST(DAOS_GET_MIN);
	CONST(DAOS_GET_DKEY);
	CONST(DAOS_GET_AKEY);
	CONST(DAOS_GET_RECX);
	CONST(MAX_OBJ_CLASS_NAME_LEN);
	CONST(OR_RS_2P1);
	CONST(MAX_NUM_GROUPS);
	CONST(DAOS_TF_RDONLY);
	CONST(DAOS_TF_ZERO_COPY);

	/* Object types and the compiled-in class ids (crate::obj). */
	OBJ_TYPE(DAOS_OT_MULTI_HASHED);
	OBJ_TYPE(DAOS_OT_DKEY_UINT64);
	OBJ_TYPE(DAOS_OT_AKEY_UINT64);
	OBJ_TYPE(DAOS_OT_MULTI_UINT64);
	OBJ_TYPE(DAOS_OT_DKEY_LEXICAL);
	OBJ_TYPE(DAOS_OT_AKEY_LEXICAL);
	OBJ_TYPE(DAOS_OT_MULTI_LEXICAL);
	OBJ_TYPE(DAOS_OT_KV_HASHED);
	OBJ_TYPE(DAOS_OT_KV_UINT64);
	OBJ_TYPE(DAOS_OT_KV_LEXICAL);
	OBJ_TYPE(DAOS_OT_ARRAY);
	OBJ_TYPE(DAOS_OT_ARRAY_ATTR);
	OBJ_TYPE(DAOS_OT_ARRAY_BYTE);
	CONST(OC_S1);
	CONST(OC_SX);
	CONST(OC_RP_2G1);
	CONST(OC_RP_2GX);
	CONST(OC_RP_3G1);
	CONST(OC_RP_3GX);
	CONST(OC_EC_2P1G1);
	CONST(OC_EC_2P1GX);
	CONST(OC_EC_4P1G1);
	CONST(OC_EC_4P1GX);
	CONST(OC_EC_4P2G1);
	CONST(OC_EC_4P2GX);
	CONST(OC_EC_8P2G1);
	CONST(OC_EC_8P2GX);
	CONST(OC_EC_16P2G1);
	CONST(OC_EC_16P2GX);
	CONST(DAOS_OCH_RDD_DEF);
	CONST(DAOS_OCH_RDD_NO);
	CONST(DAOS_OCH_RDD_RP);
	CONST(DAOS_OCH_RDD_EC);
	CONST(DAOS_OCH_SHD_DEF);
	CONST(DAOS_OCH_SHD_TINY);
	CONST(DAOS_OCH_SHD_REG);
	CONST(DAOS_OCH_SHD_HI);
	CONST(DAOS_OCH_SHD_EXT);
	CONST(DAOS_OCH_SHD_MAX);
	/*
	 * Object id vectors: the upper 32 bits of oid.hi built with the public
	 * OID_FMT_* shifts, decoded with the public inline functions (and the
	 * OID_FMT_META_* mask for the group count: daos_obj_id2grp_nr is absent
	 * from the 2.6.0 headers).
	 */
	{
		static const struct {
			uint64_t type, ord, grp, low;
		} vec[] = {
			{DAOS_OT_KV_HASHED, OR_RP_1, 1, 0x12345678},
			{DAOS_OT_ARRAY_BYTE, OR_RS_4P1, MAX_NUM_GROUPS, 0x9abcdef0},
			{DAOS_OT_MULTI_UINT64, OR_RP_3, 16, 0},
		};
		for (size_t i = 0; i < sizeof(vec) / sizeof(vec[0]); i++) {
			daos_obj_id_t oid = {.lo = 1};

			oid.hi = (vec[i].type << OID_FMT_TYPE_SHIFT) | (vec[i].ord << OID_FMT_CLASS_SHIFT) |
				 (vec[i].grp << OID_FMT_META_SHIFT) | vec[i].low;
			printf("oid %llu %d %u %u\n", (unsigned long long)oid.hi,
			       (int)daos_obj_id2type(oid), (unsigned)daos_obj_id2class(oid),
			       (unsigned)((oid.hi & OID_FMT_META_MASK) >> OID_FMT_META_SHIFT));
		}
	}

	D_FOREACH_GURT_ERR(DER)
	D_FOREACH_DAOS_ERR(DER)
	return 0;
}
