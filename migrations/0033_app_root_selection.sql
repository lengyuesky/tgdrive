-- 只在卸载当前根地址应用时原子清除选择，不改变无关应用卸载时的选择修订。
DROP TRIGGER app_root_uninstall_revision;
CREATE TRIGGER app_root_uninstall_revision BEFORE DELETE ON app_installations BEGIN
    UPDATE app_root_selection SET app_id=NULL,revision=lower(hex(randomblob(16))) WHERE id=1 AND app_id=OLD.id;
END;
